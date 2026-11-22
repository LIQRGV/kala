//! Story-time ticks and their order-preserving key encoding.
//!
//! A tick is a signed `i64` in the world's own calendar; what one tick means is
//! decided by the app, never by the store. In index keys a tick is stored as a
//! sign-flipped big-endian integer, so byte order equals numeric order
//! (negative ticks included). The inverted form (`^tick`) is the bitwise NOT of
//! that, so the newest tick sorts first and a forward seek to `^T` lands on the
//! latest entry with tick `<= T`.

/// A point on the story timeline.
///
/// Ordering is numeric, matching the byte order of [`Tick::encode`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Tick(pub i64);

/// Flipping the sign bit maps `i64::MIN..=i64::MAX` onto `0..=u64::MAX`
/// monotonically, which is what makes big-endian bytes sort numerically.
const SIGN_BIT: u64 = 1 << 63;

impl Tick {
    /// Encodes the tick as 8 sign-flipped big-endian bytes whose byte order
    /// equals numeric order (`i64::MIN` is all zeros, `i64::MAX` all `0xFF`).
    #[inline]
    pub fn encode(self) -> [u8; 8] {
        ((self.0 as u64) ^ SIGN_BIT).to_be_bytes()
    }

    /// Decodes bytes produced by [`Tick::encode`]. Every 8-byte input is a
    /// valid tick, so this cannot fail.
    #[inline]
    pub fn decode(bytes: [u8; 8]) -> Tick {
        Tick((u64::from_be_bytes(bytes) ^ SIGN_BIT) as i64)
    }

    /// Encodes the tick inverted (`^tick`): the bitwise NOT of
    /// [`Tick::encode`], so later ticks sort first.
    #[inline]
    pub fn encode_inverted(self) -> [u8; 8] {
        (!((self.0 as u64) ^ SIGN_BIT)).to_be_bytes()
    }

    /// Decodes bytes produced by [`Tick::encode_inverted`]. Cannot fail.
    #[inline]
    pub fn decode_inverted(bytes: [u8; 8]) -> Tick {
        Tick((!u64::from_be_bytes(bytes) ^ SIGN_BIT) as i64)
    }
}

/// How precisely an author knows a fact's tick: vague time, v1 (CLAUDE.md
/// "Vague time"). The store keeps exact ticks only; the precision is a
/// payload next to the tick (`tick = start of -208, precision = year`) for
/// display layers ("sometime in -208"). It is never part of an index key:
/// queries treat the tick as exact, and the no-op and same-tick rules ignore
/// it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
pub enum Precision {
    /// The tick is exact (the default).
    #[default]
    Exact,
    /// Known to the day.
    Day,
    /// Known to the month.
    Month,
    /// Known to the year.
    Year,
    /// Known to the decade.
    Decade,
    /// Known to the century.
    Century,
}

impl Precision {
    /// The log byte of the precision (0 exact .. 5 century; see
    /// [`crate::log`]).
    pub(crate) fn to_byte(self) -> u8 {
        match self {
            Precision::Exact => 0,
            Precision::Day => 1,
            Precision::Month => 2,
            Precision::Year => 3,
            Precision::Decade => 4,
            Precision::Century => 5,
        }
    }

    /// The precision of log byte `b`, or `None` for a byte no writer
    /// produces (above 5).
    pub(crate) fn from_byte(b: u8) -> Option<Precision> {
        Some(match b {
            0 => Precision::Exact,
            1 => Precision::Day,
            2 => Precision::Month,
            3 => Precision::Year,
            4 => Precision::Decade,
            5 => Precision::Century,
            _ => return None,
        })
    }
}

#[cfg(test)]
mod tests {
    use crate::Tick;
    use proptest::prelude::*;

    const EDGES: [i64; 11] = [
        i64::MIN,
        i64::MIN + 1,
        i64::MIN + 2,
        -256,
        -1,
        0,
        1,
        255,
        256,
        i64::MAX - 1,
        i64::MAX,
    ];

    #[test]
    fn encode_is_eight_bytes() {
        let b: [u8; 8] = Tick(0).encode();
        assert_eq!(b.len(), 8);
        let b: [u8; 8] = Tick(0).encode_inverted();
        assert_eq!(b.len(), 8);
    }

    #[test]
    fn encode_known_bytes() {
        assert_eq!(Tick(i64::MIN).encode(), [0; 8]);
        assert_eq!(Tick(0).encode(), [0x80, 0, 0, 0, 0, 0, 0, 0]);
        assert_eq!(
            Tick(-1).encode(),
            [0x7F, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF]
        );
        assert_eq!(Tick(i64::MAX).encode(), [0xFF; 8]);
    }

    #[test]
    fn inverted_is_bitwise_not_of_encode() {
        for v in EDGES {
            let e = Tick(v).encode();
            let i = Tick(v).encode_inverted();
            for k in 0..8 {
                assert_eq!(i[k], !e[k], "tick {v} byte {k}");
            }
        }
    }

    #[test]
    fn byte_order_equals_numeric_order_at_edges() {
        for &a in &EDGES {
            for &b in &EDGES {
                assert_eq!(
                    Tick(a).encode().cmp(&Tick(b).encode()),
                    a.cmp(&b),
                    "a={a} b={b}"
                );
            }
        }
    }

    #[test]
    fn inverted_sorts_newest_first_at_edges() {
        for &a in &EDGES {
            for &b in &EDGES {
                assert_eq!(
                    Tick(a).encode_inverted().cmp(&Tick(b).encode_inverted()),
                    b.cmp(&a),
                    "a={a} b={b}"
                );
            }
        }
        let mut sorted: Vec<[u8; 8]> = EDGES.iter().map(|&v| Tick(v).encode_inverted()).collect();
        sorted.sort();
        let decoded: Vec<i64> = sorted
            .into_iter()
            .map(|b| Tick::decode_inverted(b).0)
            .collect();
        let mut expected = EDGES.to_vec();
        expected.sort_by(|a, b| b.cmp(a));
        assert_eq!(decoded, expected);
    }

    #[test]
    fn round_trip_at_edges() {
        for v in EDGES {
            assert_eq!(Tick::decode(Tick(v).encode()), Tick(v));
            assert_eq!(Tick::decode_inverted(Tick(v).encode_inverted()), Tick(v));
        }
    }

    #[test]
    fn tick_ord_matches_i64() {
        assert!(Tick(-5) < Tick(3));
        assert!(Tick(i64::MIN) < Tick(i64::MAX));
        assert_eq!(Tick(7), Tick(7));
    }

    proptest! {
        #[test]
        fn prop_byte_order_equals_numeric_order(a in any::<i64>(), b in any::<i64>()) {
            prop_assert_eq!(Tick(a).cmp(&Tick(b)), Tick(a).encode().cmp(&Tick(b).encode()));
            prop_assert_eq!(a.cmp(&b), Tick(a).encode().cmp(&Tick(b).encode()));
        }

        #[test]
        fn prop_inverted_order_is_reversed(a in any::<i64>(), b in any::<i64>()) {
            prop_assert_eq!(
                Tick(b).cmp(&Tick(a)),
                Tick(a).encode_inverted().cmp(&Tick(b).encode_inverted())
            );
        }

        #[test]
        fn prop_round_trips(a in any::<i64>()) {
            prop_assert_eq!(Tick::decode(Tick(a).encode()), Tick(a));
            prop_assert_eq!(Tick::decode_inverted(Tick(a).encode_inverted()), Tick(a));
        }

        #[test]
        fn prop_decode_encode_is_identity_on_bytes(bytes in any::<[u8; 8]>()) {
            prop_assert_eq!(Tick::decode(bytes).encode(), bytes);
            prop_assert_eq!(Tick::decode_inverted(bytes).encode_inverted(), bytes);
        }
    }
}
