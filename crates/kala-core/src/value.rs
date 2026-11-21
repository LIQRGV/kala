//! Attribute values and their order-preserving byte encoding.
//!
//! The `V` index sorts values as bytes, so every value type has an encoding
//! whose byte order equals the type's natural order. An encoding is one tag
//! byte followed by the payload:
//!
//! | type     | payload                                                     |
//! |----------|-------------------------------------------------------------|
//! | `bool`   | `0x00` / `0x01`                                             |
//! | `i64`    | 8 bytes, sign-flipped big-endian                            |
//! | `f64`    | 8 bytes, sign-bit trick big-endian                           |
//! | `string` | UTF-8 with `0x00` escaped as `0x00 0xFF`, then `0x00 0x00`  |
//! | `ref`    | 8 bytes, big-endian [`EntityId`]                            |
//!
//! Strings are self-delimiting so more key bytes (entity, `^tick`, `^tx`) can
//! follow them without breaking order. Every value has exactly one encoding,
//! and [`Value::decode`] accepts only that encoding, so equal values always
//! have equal bytes.

use std::fmt;

use crate::id::EntityId;

/// Longest string (in UTF-8 bytes, as written) an indexed attribute accepts.
pub const MAX_INDEXED_STRING_BYTES: usize = 256;

/// The declared type of an attribute's values.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ValueType {
    /// `true` / `false`.
    Bool,
    /// Signed 64-bit integer.
    I64,
    /// 64-bit float; never NaN, `-0.0` normalized to `0.0`.
    F64,
    /// UTF-8 string, stored exactly as written.
    Str,
    /// Reference to an entity (or, later, an edge).
    Ref,
}

/// A single attribute value.
///
/// Build floats with [`Value::f64`], which rejects NaN and normalizes `-0.0`.
/// The `F64` variant is public, so [`Value::encode`] re-checks both.
#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    /// A boolean.
    Bool(bool),
    /// A signed 64-bit integer.
    I64(i64),
    /// A 64-bit float (never NaN once encoded).
    F64(f64),
    /// A UTF-8 string.
    Str(String),
    /// A reference to an entity.
    Ref(EntityId),
}

/// Why a value can't be encoded, decoded or indexed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ValueError {
    /// NaN is not a storable `f64`.
    NaN,
    /// A string for an indexed attribute is longer than
    /// [`MAX_INDEXED_STRING_BYTES`]; `len` is its UTF-8 length.
    StringTooLong {
        /// UTF-8 length of the rejected string.
        len: usize,
    },
    /// Bytes given to [`Value::decode`] are not a canonical value encoding.
    Malformed(&'static str),
}

impl fmt::Display for ValueError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ValueError::NaN => f.write_str("NaN is not a valid f64 value"),
            ValueError::StringTooLong { len } => write!(
                f,
                "string of {len} bytes exceeds the {MAX_INDEXED_STRING_BYTES}-byte limit for indexed attributes"
            ),
            ValueError::Malformed(why) => write!(f, "malformed value encoding: {why}"),
        }
    }
}

impl std::error::Error for ValueError {}

// Tag bytes. Their relative order only matters across types, which never meet
// in one attribute, but they are fixed forever once data is written.
const TAG_BOOL: u8 = 0x01;
const TAG_I64: u8 = 0x02;
const TAG_F64: u8 = 0x03;
const TAG_STR: u8 = 0x04;
const TAG_REF: u8 = 0x05;

const SIGN_BIT: u64 = 1 << 63;

/// String escape: a `0x00` content byte becomes `0x00 0xFF`; the string ends
/// with `0x00 0x00`. Since the terminator's second byte is lower than any
/// escape or content continuation, a shorter string sorts before any longer
/// one it prefixes, whatever bytes follow the encoding.
const STR_ESCAPE: u8 = 0xFF;
const STR_END: u8 = 0x00;

/// Standard sign-bit trick: positive floats get the sign bit flipped,
/// negative floats get every bit flipped, so unsigned order equals numeric
/// order. Callers must have rejected NaN and normalized `-0.0`.
#[inline]
fn f64_to_key(x: f64) -> u64 {
    let bits = x.to_bits();
    if bits & SIGN_BIT == 0 {
        bits ^ SIGN_BIT
    } else {
        !bits
    }
}

/// Inverse of [`f64_to_key`].
#[inline]
fn f64_from_key(key: u64) -> f64 {
    if key & SIGN_BIT != 0 {
        f64::from_bits(key ^ SIGN_BIT)
    } else {
        f64::from_bits(!key)
    }
}

/// Rejects NaN and maps `-0.0` to `0.0`, so equal numbers have equal bytes.
#[inline]
fn normalize_f64(x: f64) -> Result<f64, ValueError> {
    if x.is_nan() {
        Err(ValueError::NaN)
    } else if x == 0.0 {
        Ok(0.0)
    } else {
        Ok(x)
    }
}

/// Reads exactly 8 payload bytes, failing on anything shorter or longer.
#[inline]
fn fixed8(payload: &[u8]) -> Result<[u8; 8], ValueError> {
    match <[u8; 8]>::try_from(payload) {
        Ok(b) => Ok(b),
        Err(_) if payload.len() < 8 => Err(ValueError::Malformed("truncated payload")),
        Err(_) => Err(ValueError::Malformed("trailing bytes")),
    }
}

/// Decodes an escaped string starting at `bytes[0]`; returns the string and
/// the number of bytes consumed (terminator included).
fn decode_str(bytes: &[u8]) -> Result<(String, usize), ValueError> {
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    loop {
        match bytes.get(i) {
            None => return Err(ValueError::Malformed("unterminated string")),
            Some(&0x00) => match bytes.get(i + 1) {
                Some(&STR_END) => break,
                Some(&STR_ESCAPE) => {
                    out.push(0x00);
                    i += 2;
                }
                Some(_) => return Err(ValueError::Malformed("bad string escape")),
                None => return Err(ValueError::Malformed("unterminated string")),
            },
            Some(&b) => {
                out.push(b);
                i += 1;
            }
        }
    }
    let s = String::from_utf8(out).map_err(|_| ValueError::Malformed("invalid UTF-8"))?;
    Ok((s, i + 2))
}

/// The length of the value encoding at the start of `bytes`, which may be
/// followed by more key bytes. Only finds the boundary (tag, fixed width or
/// string terminator); it doesn't check the payload is canonical, which
/// [`Value::decode`] does on exactly the returned slice.
pub(crate) fn encoded_len(bytes: &[u8]) -> Result<usize, ValueError> {
    let (&tag, payload) = bytes
        .split_first()
        .ok_or(ValueError::Malformed("empty input"))?;
    let payload_len = match tag {
        TAG_BOOL => 1,
        TAG_I64 | TAG_F64 | TAG_REF => 8,
        TAG_STR => {
            // Scan for the terminator without building the string: 0x00 is
            // always followed by STR_ESCAPE (literal NUL) or STR_END.
            let mut i = 0;
            loop {
                match payload.get(i) {
                    None => return Err(ValueError::Malformed("unterminated string")),
                    Some(&0x00) => match payload.get(i + 1) {
                        Some(&STR_END) => break i + 2,
                        Some(&STR_ESCAPE) => i += 2,
                        Some(_) => return Err(ValueError::Malformed("bad string escape")),
                        None => return Err(ValueError::Malformed("unterminated string")),
                    },
                    Some(_) => i += 1,
                }
            }
        }
        _ => return Err(ValueError::Malformed("unknown type tag")),
    };
    if payload.len() < payload_len {
        return Err(ValueError::Malformed("truncated payload"));
    }
    Ok(1 + payload_len)
}

impl Value {
    /// Builds an `F64` value: NaN is rejected, `-0.0` becomes `0.0`.
    pub fn f64(x: f64) -> Result<Value, ValueError> {
        normalize_f64(x).map(Value::F64)
    }

    /// The type of this value.
    pub fn value_type(&self) -> ValueType {
        match self {
            Value::Bool(_) => ValueType::Bool,
            Value::I64(_) => ValueType::I64,
            Value::F64(_) => ValueType::F64,
            Value::Str(_) => ValueType::Str,
            Value::Ref(_) => ValueType::Ref,
        }
    }

    /// Checks the limits for an indexed attribute: a string may be at most
    /// [`MAX_INDEXED_STRING_BYTES`] UTF-8 bytes. Other types always pass.
    pub fn check_indexed(&self) -> Result<(), ValueError> {
        match self {
            Value::Str(s) if s.len() > MAX_INDEXED_STRING_BYTES => {
                Err(ValueError::StringTooLong { len: s.len() })
            }
            _ => Ok(()),
        }
    }

    /// Encodes the value as a tag byte plus an order-preserving payload.
    ///
    /// Fails only for an `F64` holding NaN. Encoding has no string length
    /// limit; that is [`Value::check_indexed`]'s job.
    pub fn encode(&self) -> Result<Vec<u8>, ValueError> {
        let out = match self {
            Value::Bool(b) => vec![TAG_BOOL, u8::from(*b)],
            Value::I64(n) => {
                let mut out = Vec::with_capacity(9);
                out.push(TAG_I64);
                out.extend_from_slice(&((*n as u64) ^ SIGN_BIT).to_be_bytes());
                out
            }
            Value::F64(x) => {
                let mut out = Vec::with_capacity(9);
                out.push(TAG_F64);
                out.extend_from_slice(&f64_to_key(normalize_f64(*x)?).to_be_bytes());
                out
            }
            Value::Str(s) => {
                let bytes = s.as_bytes();
                let mut out = Vec::with_capacity(bytes.len() + 3);
                out.push(TAG_STR);
                for &b in bytes {
                    out.push(b);
                    if b == 0x00 {
                        out.push(STR_ESCAPE);
                    }
                }
                out.extend_from_slice(&[0x00, STR_END]);
                out
            }
            Value::Ref(id) => {
                let mut out = Vec::with_capacity(9);
                out.push(TAG_REF);
                out.extend_from_slice(&id.to_bytes());
                out
            }
        };
        Ok(out)
    }

    /// Decodes exactly one value from `bytes`, which must be its canonical
    /// encoding with nothing after it. Never panics; any other input
    /// (unknown tag, truncation, trailing bytes, NaN, `-0.0`, bad escape,
    /// invalid UTF-8) is an error.
    pub fn decode(bytes: &[u8]) -> Result<Value, ValueError> {
        let (&tag, payload) = bytes
            .split_first()
            .ok_or(ValueError::Malformed("empty input"))?;
        match tag {
            TAG_BOOL => match payload {
                [0] => Ok(Value::Bool(false)),
                [1] => Ok(Value::Bool(true)),
                [] => Err(ValueError::Malformed("truncated payload")),
                [_] => Err(ValueError::Malformed("bool payload not 0 or 1")),
                _ => Err(ValueError::Malformed("trailing bytes")),
            },
            TAG_I64 => {
                let raw = u64::from_be_bytes(fixed8(payload)?);
                Ok(Value::I64((raw ^ SIGN_BIT) as i64))
            }
            TAG_F64 => {
                let x = f64_from_key(u64::from_be_bytes(fixed8(payload)?));
                if x.is_nan() {
                    return Err(ValueError::NaN);
                }
                // -0.0 is never written, so its bytes are not canonical.
                if x == 0.0 && x.is_sign_negative() {
                    return Err(ValueError::Malformed("non-canonical -0.0"));
                }
                Ok(Value::F64(x))
            }
            TAG_STR => {
                let (s, used) = decode_str(payload)?;
                if used != payload.len() {
                    return Err(ValueError::Malformed("trailing bytes"));
                }
                Ok(Value::Str(s))
            }
            TAG_REF => Ok(Value::Ref(EntityId::from_bytes(fixed8(payload)?))),
            _ => Err(ValueError::Malformed("unknown type tag")),
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::id::EntityId;
    use crate::{MAX_INDEXED_STRING_BYTES, Value, ValueError, ValueType};
    use proptest::prelude::*;
    use std::cmp::Ordering;

    fn enc(v: &Value) -> Vec<u8> {
        v.encode().expect("encodable")
    }

    fn s(x: &str) -> Value {
        Value::Str(x.to_string())
    }

    fn f(x: f64) -> Value {
        Value::f64(x).expect("not NaN")
    }

    fn rf(n: u64) -> Value {
        Value::Ref(EntityId::from_bytes(n.to_be_bytes()))
    }

    fn assert_round_trip(v: Value) {
        let bytes = enc(&v);
        assert_eq!(Value::decode(&bytes), Ok(v));
    }

    // ---- types ----

    #[test]
    fn value_type_of_each_variant() {
        assert_eq!(Value::Bool(true).value_type(), ValueType::Bool);
        assert_eq!(Value::I64(1).value_type(), ValueType::I64);
        assert_eq!(f(1.5).value_type(), ValueType::F64);
        assert_eq!(s("x").value_type(), ValueType::Str);
        assert_eq!(rf(1).value_type(), ValueType::Ref);
    }

    #[test]
    fn each_type_has_distinct_tag_byte() {
        let tags: Vec<u8> = [Value::Bool(false), Value::I64(0), f(0.0), s(""), rf(1)]
            .iter()
            .map(|v| enc(v)[0])
            .collect();
        let mut dedup = tags.clone();
        dedup.sort();
        dedup.dedup();
        assert_eq!(dedup.len(), 5, "tags {tags:?}");
    }

    // ---- round trips ----

    #[test]
    fn round_trip_bool() {
        assert_round_trip(Value::Bool(false));
        assert_round_trip(Value::Bool(true));
    }

    #[test]
    fn round_trip_i64_edges() {
        for v in [i64::MIN, i64::MIN + 1, -1, 0, 1, i64::MAX - 1, i64::MAX] {
            assert_round_trip(Value::I64(v));
        }
    }

    #[test]
    fn round_trip_f64_edges() {
        for v in [
            f64::NEG_INFINITY,
            f64::MIN,
            -1.5,
            -f64::MIN_POSITIVE,
            -5e-324,
            0.0,
            5e-324,
            f64::MIN_POSITIVE,
            1.5,
            f64::MAX,
            f64::INFINITY,
        ] {
            assert_round_trip(f(v));
        }
    }

    #[test]
    fn round_trip_strings() {
        for v in [
            "",
            "a",
            "Edwin",
            "a\0b",
            "\0",
            "\0\0",
            "a\0",
            "é",
            "日本語",
            "😀",
            "\u{FFFF}",
        ] {
            assert_round_trip(s(v));
        }
    }

    #[test]
    fn round_trip_ref() {
        for n in [0u64, 1, 0x0001_0000_0000_0001, u64::MAX >> 1] {
            assert_round_trip(rf(n));
        }
    }

    // ---- bool / i64 order ----

    #[test]
    fn bool_false_before_true() {
        assert!(enc(&Value::Bool(false)) < enc(&Value::Bool(true)));
    }

    #[test]
    fn i64_order_at_edges() {
        let vals = [i64::MIN, i64::MIN + 1, -1, 0, 1, i64::MAX - 1, i64::MAX];
        for w in vals.windows(2) {
            assert!(enc(&Value::I64(w[0])) < enc(&Value::I64(w[1])), "{w:?}");
        }
    }

    #[test]
    fn ref_order_follows_raw_id() {
        assert!(enc(&rf(1)) < enc(&rf(2)));
        assert!(enc(&rf(0x0001_0000_0000_0005)) < enc(&rf(0x0002_0000_0000_0001)));
    }

    // ---- f64 ----

    #[test]
    fn f64_constructor_rejects_nan() {
        assert_eq!(Value::f64(f64::NAN), Err(ValueError::NaN));
        assert_eq!(Value::f64(-f64::NAN), Err(ValueError::NaN));
    }

    #[test]
    fn f64_encode_rejects_nan_built_directly() {
        assert_eq!(Value::F64(f64::NAN).encode(), Err(ValueError::NaN));
    }

    #[test]
    fn f64_constructor_normalizes_negative_zero() {
        match Value::f64(-0.0).unwrap() {
            Value::F64(x) => {
                assert_eq!(x, 0.0);
                assert!(x.is_sign_positive(), "-0.0 must be stored as 0.0");
            }
            other => panic!("expected F64, got {other:?}"),
        }
    }

    #[test]
    fn f64_encode_normalizes_negative_zero_built_directly() {
        assert_eq!(enc(&Value::F64(-0.0)), enc(&Value::F64(0.0)));
        assert_eq!(enc(&f(-0.0)), enc(&f(0.0)));
    }

    #[test]
    fn f64_decoded_zero_is_positive_zero() {
        match Value::decode(&enc(&Value::F64(-0.0))).unwrap() {
            Value::F64(x) => assert!(x.is_sign_positive()),
            other => panic!("expected F64, got {other:?}"),
        }
    }

    #[test]
    fn f64_order_chain() {
        let chain = [
            f64::NEG_INFINITY,
            f64::MIN,
            -1.0,
            -f64::MIN_POSITIVE,
            -5e-324,
            0.0,
            5e-324,
            f64::MIN_POSITIVE,
            1.0,
            f64::MAX,
            f64::INFINITY,
        ];
        for w in chain.windows(2) {
            assert!(enc(&f(w[0])) < enc(&f(w[1])), "{w:?}");
        }
    }

    #[test]
    fn f64_negative_before_positive() {
        assert!(enc(&f(-1e-300)) < enc(&f(1e-300)));
        assert!(enc(&f(-2.0)) < enc(&f(-1.0)));
        assert!(enc(&f(1.0)) < enc(&f(2.0)));
    }

    #[test]
    fn f64_subnormals_distinct_and_ordered() {
        let a = 5e-324; // smallest subnormal
        let b = 1e-320;
        assert!(enc(&f(0.0)) < enc(&f(a)));
        assert!(enc(&f(a)) < enc(&f(b)));
        assert!(enc(&f(b)) < enc(&f(f64::MIN_POSITIVE)));
        assert!(enc(&f(-f64::MIN_POSITIVE)) < enc(&f(-b)));
        assert!(enc(&f(-b)) < enc(&f(-a)));
        assert!(enc(&f(-a)) < enc(&f(0.0)));
    }

    // ---- strings ----

    #[test]
    fn string_order_prefix_relations() {
        let chain = [
            "", "\0", "\0\0", "\0a", "a", "a\0", "a\0\0", "a\0b", "aa", "ab", "b",
        ];
        for w in chain.windows(2) {
            assert_eq!(
                w[0].as_bytes().cmp(w[1].as_bytes()),
                Ordering::Less,
                "test chain itself must be sorted: {w:?}"
            );
            assert!(enc(&s(w[0])) < enc(&s(w[1])), "{w:?}");
        }
    }

    #[test]
    fn string_a_lt_a_nul_lt_ab() {
        assert!(enc(&s("a")) < enc(&s("a\0")));
        assert!(enc(&s("a\0")) < enc(&s("ab")));
    }

    #[test]
    fn empty_string_encodes_and_sorts_first() {
        assert!(enc(&s("")) < enc(&s("\0")));
        assert!(enc(&s("")) < enc(&s("a")));
    }

    #[test]
    fn string_is_self_delimiting_under_suffix() {
        let mut a = enc(&s("a"));
        a.extend_from_slice(&[0xFF; 8]);
        let mut ab = enc(&s("ab"));
        ab.extend_from_slice(&[0x00; 8]);
        assert!(a < ab);

        let mut empty = enc(&s(""));
        empty.extend_from_slice(&[0xFF; 8]);
        let mut nul = enc(&s("\0"));
        nul.extend_from_slice(&[0x00; 8]);
        assert!(empty < nul);

        let mut anul = enc(&s("a\0"));
        anul.extend_from_slice(&[0x00; 8]);
        let mut a2 = enc(&s("a"));
        a2.extend_from_slice(&[0xFF; 8]);
        assert!(a2 < anul);
    }

    #[test]
    fn string_encoding_is_a_prefix_free_code() {
        let vals = ["", "a", "a\0", "ab", "\0", "\0\0", "b"];
        for x in vals {
            for y in vals {
                if x != y {
                    let (ex, ey) = (enc(&s(x)), enc(&s(y)));
                    assert!(!ey.starts_with(&ex), "{x:?} encoding is a prefix of {y:?}");
                }
            }
        }
    }

    // ---- decode errors ----

    #[test]
    fn decode_rejects_empty() {
        assert!(Value::decode(&[]).is_err());
    }

    #[test]
    fn decode_rejects_unknown_tag() {
        let valid_tags: Vec<u8> = [Value::Bool(false), Value::I64(0), f(0.0), s(""), rf(1)]
            .iter()
            .map(|v| enc(v)[0])
            .collect();
        let unknown = (0u8..=255).find(|t| !valid_tags.contains(t)).unwrap();
        let mut bytes = vec![unknown];
        bytes.extend_from_slice(&[0; 8]);
        assert!(Value::decode(&bytes).is_err());
        assert!(Value::decode(&[0xFF]).is_err() || valid_tags.contains(&0xFF));
    }

    #[test]
    fn decode_rejects_truncated_fixed_width_payloads() {
        for v in [Value::Bool(true), Value::I64(-7), f(2.5), rf(9)] {
            let bytes = enc(&v);
            for cut in 1..bytes.len() {
                assert!(
                    Value::decode(&bytes[..cut]).is_err(),
                    "{v:?} truncated to {cut} bytes"
                );
            }
        }
    }

    #[test]
    fn decode_rejects_trailing_bytes() {
        for v in [Value::Bool(true), Value::I64(-7), f(2.5), rf(9), s("hi")] {
            let mut bytes = enc(&v);
            bytes.push(0x42);
            assert!(Value::decode(&bytes).is_err(), "{v:?} with trailing byte");
        }
    }

    #[test]
    fn decode_rejects_bad_bool_payload() {
        let mut bytes = enc(&Value::Bool(true));
        *bytes.last_mut().unwrap() = 2;
        assert!(Value::decode(&bytes).is_err());
    }

    #[test]
    fn decode_rejects_nan_f64_payload() {
        // Encode NaN-like bit pattern by hand: take the encoding of 1.0 and
        // replace the payload with the order-encoded quiet NaN (sign bit flipped).
        let mut bytes = enc(&f(1.0));
        let nan_bits = f64::NAN.to_bits() ^ (1u64 << 63);
        bytes.splice(1.., nan_bits.to_be_bytes());
        assert!(Value::decode(&bytes).is_err());
    }

    #[test]
    fn decode_rejects_negative_zero_bytes() {
        // -0.0 is normalized on write, so its order-encoding is non-canonical.
        // Order-encoded -0.0: sign set, so all bits flipped = 0x7FFF_FFFF_FFFF_FFFF.
        let bytes = [0x03, 0x7F, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF];
        assert!(Value::decode(&bytes).is_err());
    }

    #[test]
    fn decode_rejects_negative_sign_nan_payload() {
        // NaN with the sign bit set (0xFFF8...), order-encoded: negative => flip all bits.
        let nan_bits = (f64::NAN.to_bits() | (1u64 << 63)) ^ u64::MAX;
        let mut bytes = vec![0x03];
        bytes.extend_from_slice(&nan_bits.to_be_bytes());
        assert!(Value::decode(&bytes).is_err());
        // Also a negative-sign NaN with a different payload.
        let other = (0xFFF0_0000_0000_0001u64) ^ u64::MAX;
        let mut bytes = vec![0x03];
        bytes.extend_from_slice(&other.to_be_bytes());
        assert!(Value::decode(&bytes).is_err());
    }

    #[test]
    fn decode_rejects_tag_zero() {
        assert!(Value::decode(&[0x00]).is_err());
        assert!(Value::decode(&[0x00, 0x00]).is_err());
        assert!(
            Value::decode(&[0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01]).is_err()
        );
    }

    #[test]
    fn decode_rejects_unterminated_string() {
        let bytes = enc(&s("abc"));
        // drop the 0x00 0x00 terminator
        assert!(Value::decode(&bytes[..bytes.len() - 2]).is_err());
        // drop only the last terminator byte
        assert!(Value::decode(&bytes[..bytes.len() - 1]).is_err());
        // tag only
        assert!(Value::decode(&bytes[..1]).is_err());
    }

    #[test]
    fn decode_rejects_bad_string_escape() {
        // 0x00 must be followed by 0xFF (escaped NUL) or 0x00 (terminator).
        let tag = enc(&s(""))[0];
        assert!(Value::decode(&[tag, b'a', 0x00, 0x01, 0x00, 0x00]).is_err());
    }

    #[test]
    fn decode_rejects_invalid_utf8() {
        let tag = enc(&s(""))[0];
        // lone continuation byte, then terminator
        assert!(Value::decode(&[tag, 0x80, 0x00, 0x00]).is_err());
        // truncated 2-byte sequence
        assert!(Value::decode(&[tag, 0xC3, 0x00, 0x00]).is_err());
        // overlong NUL / 0xFF byte
        assert!(Value::decode(&[tag, 0xFF, 0x00, 0x00]).is_err());
    }

    // ---- check_indexed ----

    #[test]
    fn indexed_limit_constant_is_256() {
        assert_eq!(MAX_INDEXED_STRING_BYTES, 256);
    }

    #[test]
    fn check_indexed_accepts_256_bytes() {
        assert_eq!(s(&"a".repeat(256)).check_indexed(), Ok(()));
    }

    #[test]
    fn check_indexed_rejects_257_bytes() {
        assert_eq!(
            s(&"a".repeat(257)).check_indexed(),
            Err(ValueError::StringTooLong { len: 257 })
        );
    }

    #[test]
    fn check_indexed_counts_utf8_bytes_not_chars() {
        // 128 x "é" = 256 bytes (128 chars): ok
        assert_eq!(s(&"é".repeat(128)).check_indexed(), Ok(()));
        // 129 x "é" = 258 bytes (129 chars): rejected
        assert_eq!(
            s(&"é".repeat(129)).check_indexed(),
            Err(ValueError::StringTooLong { len: 258 })
        );
    }

    #[test]
    fn check_indexed_accepts_empty_and_non_strings() {
        assert_eq!(s("").check_indexed(), Ok(()));
        assert_eq!(Value::Bool(true).check_indexed(), Ok(()));
        assert_eq!(Value::I64(i64::MIN).check_indexed(), Ok(()));
        assert_eq!(f(1.0).check_indexed(), Ok(()));
        assert_eq!(rf(1).check_indexed(), Ok(()));
    }

    #[test]
    fn long_string_still_encodes_and_round_trips() {
        // The limit is a separate check; encoding itself has no length limit.
        assert_round_trip(s(&"x".repeat(100_000)));
    }

    // ---- properties ----

    proptest! {
        #[test]
        fn prop_i64_round_trip_and_order(a in any::<i64>(), b in any::<i64>()) {
            let (va, vb) = (Value::I64(a), Value::I64(b));
            prop_assert_eq!(Value::decode(&enc(&va)), Ok(va.clone()));
            prop_assert_eq!(a.cmp(&b), enc(&va).cmp(&enc(&vb)));
        }

        #[test]
        fn prop_f64_round_trip_and_order(
            a in any::<f64>().prop_filter("not NaN", |x| !x.is_nan()),
            b in any::<f64>().prop_filter("not NaN", |x| !x.is_nan()),
        ) {
            let (va, vb) = (f(a), f(b));
            prop_assert_eq!(Value::decode(&enc(&va)), Ok(va.clone()));
            // -0.0 == 0.0 under partial_cmp, matching normalized bytes.
            prop_assert_eq!(a.partial_cmp(&b).unwrap(), enc(&va).cmp(&enc(&vb)));
        }

        #[test]
        fn prop_string_round_trip_and_order(a in any::<String>(), b in any::<String>()) {
            let (va, vb) = (s(&a), s(&b));
            prop_assert_eq!(Value::decode(&enc(&va)), Ok(va.clone()));
            prop_assert_eq!(a.as_bytes().cmp(b.as_bytes()), enc(&va).cmp(&enc(&vb)));
        }

        #[test]
        fn prop_string_with_nuls_round_trip_and_order(
            a in "[a\u{0}b]{0,6}",
            b in "[a\u{0}b]{0,6}",
        ) {
            let (va, vb) = (s(&a), s(&b));
            prop_assert_eq!(Value::decode(&enc(&va)), Ok(va.clone()));
            prop_assert_eq!(a.as_bytes().cmp(b.as_bytes()), enc(&va).cmp(&enc(&vb)));
        }

        #[test]
        fn prop_string_order_holds_with_arbitrary_suffix(
            a in "[a\u{0}b]{0,5}",
            b in "[a\u{0}b]{0,5}",
            sa in proptest::collection::vec(any::<u8>(), 0..8),
            sb in proptest::collection::vec(any::<u8>(), 0..8),
        ) {
            prop_assume!(a != b);
            let mut ea = enc(&s(&a));
            ea.extend_from_slice(&sa);
            let mut eb = enc(&s(&b));
            eb.extend_from_slice(&sb);
            prop_assert_eq!(a.as_bytes().cmp(b.as_bytes()), ea.cmp(&eb));
        }

        #[test]
        fn prop_bool_round_trip(a in any::<bool>()) {
            let v = Value::Bool(a);
            prop_assert_eq!(Value::decode(&enc(&v)), Ok(v.clone()));
        }

        #[test]
        fn prop_ref_round_trip_and_order(a in any::<u64>(), b in any::<u64>()) {
            let (va, vb) = (rf(a), rf(b));
            prop_assert_eq!(Value::decode(&enc(&va)), Ok(va.clone()));
            prop_assert_eq!(a.cmp(&b), enc(&va).cmp(&enc(&vb)));
        }

        #[test]
        fn prop_decode_never_panics(bytes in proptest::collection::vec(any::<u8>(), 0..64)) {
            let _ = Value::decode(&bytes);
        }

        #[test]
        fn prop_decode_ok_implies_canonical(bytes in proptest::collection::vec(any::<u8>(), 0..24)) {
            // Anything that decodes must re-encode to the identical bytes
            // (one encoding per value, so equal values have equal bytes).
            if let Ok(v) = Value::decode(&bytes) {
                prop_assert_eq!(enc(&v), bytes);
            }
        }

        #[test]
        fn prop_check_indexed_matches_byte_length(x in any::<String>()) {
            let r = s(&x).check_indexed();
            if x.len() <= 256 {
                prop_assert_eq!(r, Ok(()));
            } else {
                prop_assert_eq!(r, Err(ValueError::StringTooLong { len: x.len() }));
            }
        }
    }
}

#[cfg(test)]
mod encoded_len_tests {
    use super::{Value, ValueError, encoded_len};
    use crate::id::EntityId;

    fn enc(v: &Value) -> Vec<u8> {
        v.encode().unwrap()
    }

    #[test]
    fn empty_input_is_error() {
        assert!(encoded_len(&[]).is_err());
    }

    #[test]
    fn every_tag_reports_its_own_length_ignoring_trailing_bytes() {
        let vals = [
            Value::Bool(true),
            Value::Bool(false),
            Value::I64(i64::MIN),
            Value::I64(i64::MAX),
            Value::F64(-1.25),
            Value::Ref(EntityId::from_bytes(7u64.to_be_bytes())),
            Value::Str(String::new()),
            Value::Str("hello".into()),
        ];
        for v in vals {
            let e = enc(&v);
            assert_eq!(encoded_len(&e), Ok(e.len()), "{v:?}");
            let mut more = e.clone();
            more.extend_from_slice(&[0xAA, 0x00, 0x00, 0x01]);
            assert_eq!(encoded_len(&more), Ok(e.len()), "{v:?} + trailing");
        }
    }

    #[test]
    fn fixed_lengths() {
        assert_eq!(encoded_len(&enc(&Value::Bool(true))), Ok(2));
        assert_eq!(encoded_len(&enc(&Value::I64(0))), Ok(9));
        assert_eq!(encoded_len(&enc(&Value::F64(0.0))), Ok(9));
        assert_eq!(
            encoded_len(&enc(&Value::Ref(EntityId::from_bytes([
                0, 1, 0, 0, 0, 0, 0, 1
            ])))),
            Ok(9)
        );
    }

    #[test]
    fn truncated_payloads_are_errors() {
        for v in [
            Value::Bool(true),
            Value::I64(1),
            Value::F64(1.0),
            Value::Ref(EntityId::from_bytes(1u64.to_be_bytes())),
        ] {
            let e = enc(&v);
            for cut in 1..e.len() {
                assert!(encoded_len(&e[..cut]).is_err(), "{v:?} cut to {cut}");
            }
        }
    }

    #[test]
    fn string_with_escaped_nuls_followed_by_more_bytes() {
        let v = Value::Str("a\0b\0\0c".into());
        let e = enc(&v);
        assert_eq!(encoded_len(&e), Ok(e.len()));
        let mut more = e.clone();
        more.extend_from_slice(b"\x00\x00\x00\xFFzzz");
        assert_eq!(encoded_len(&more), Ok(e.len()));
        // a string that is entirely NULs
        let n = Value::Str("\0\0\0".into());
        let mut en = enc(&n);
        let l = en.len();
        en.extend_from_slice(&[0x04, 0x00, 0x00]);
        assert_eq!(encoded_len(&en), Ok(l));
    }

    #[test]
    fn unterminated_strings_are_errors() {
        assert!(encoded_len(&[0x04]).is_err());
        assert!(encoded_len(b"\x04abc").is_err());
        // lone trailing 0x00 (escape/terminator byte missing)
        assert!(encoded_len(b"\x04abc\x00").is_err());
        // escaped NUL but then no terminator
        assert!(encoded_len(b"\x04ab\x00\xFF").is_err());
    }

    #[test]
    fn bad_escape_is_error() {
        assert!(matches!(
            encoded_len(b"\x04a\x00\x01b\x00\x00"),
            Err(ValueError::Malformed(_))
        ));
        assert!(encoded_len(b"\x04\x00\x02\x00\x00").is_err());
    }

    #[test]
    fn unknown_tags_are_errors() {
        for t in [0x00u8, 0x06, 0x7F, 0xFF] {
            assert!(
                encoded_len(&[t, 0, 0, 0, 0, 0, 0, 0, 0]).is_err(),
                "tag {t}"
            );
        }
    }
}
