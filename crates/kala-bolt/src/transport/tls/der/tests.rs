//! The DER walk against certificates rcgen produced, and against garbage.
use super::*;
use proptest::prelude::*;
use rcgen::{CertificateParams, KeyPair, date_time_ymd};

fn certificate(params: &CertificateParams) -> Vec<u8> {
    let key = KeyPair::generate().unwrap();
    params.self_signed(&key).unwrap().der().to_vec()
}

fn params() -> CertificateParams {
    CertificateParams::new(vec!["localhost".into()]).unwrap()
}

/// The tag of each of the two validity times, found by the encoding of the
/// year the test asked for (rcgen picks UTCTime before 2050).
fn time_tags(der: &[u8], (before, after): (Time, Time)) -> (u8, u8) {
    let find = |time: Time| {
        let text = time.to_string();
        let compact: String = text.chars().filter(char::is_ascii_digit).collect();
        let general = format!("{compact}Z");
        let utc = &general[2..];
        if der
            .windows(general.len() + 2)
            .any(|w| w[0] == GENERALIZED_TIME && w[1] == 15 && &w[2..] == general.as_bytes())
        {
            GENERALIZED_TIME
        } else if der
            .windows(utc.len() + 2)
            .any(|w| w[0] == UTC_TIME && w[1] == 13 && &w[2..] == utc.as_bytes())
        {
            UTC_TIME
        } else {
            panic!("{text} is not in the certificate")
        }
    };
    (find(before), find(after))
}

#[test]
fn validity_matches_what_rcgen_was_asked_for_in_both_time_encodings() {
    // (not before, not after) and the encodings RFC 5280 requires for them.
    for ((y1, m1, d1), (y2, m2, d2), tags) in [
        ((1998, 1, 1), (1999, 12, 31), (UTC_TIME, UTC_TIME)),
        ((1950, 1, 1), (2049, 12, 31), (UTC_TIME, UTC_TIME)),
        ((2024, 2, 29), (2026, 3, 1), (UTC_TIME, UTC_TIME)),
        ((2049, 12, 31), (2050, 1, 1), (UTC_TIME, GENERALIZED_TIME)),
        (
            (2100, 2, 28),
            (2101, 3, 1),
            (GENERALIZED_TIME, GENERALIZED_TIME),
        ),
        ((1975, 6, 15), (9999, 12, 31), (UTC_TIME, GENERALIZED_TIME)),
    ] {
        let mut params = params();
        params.not_before = date_time_ymd(y1, m1, d1);
        params.not_after = date_time_ymd(y2, m2, d2);
        let der = certificate(&params);
        assert!(is_certificate(&der));
        let (before, after) = validity(&der).expect("an rcgen certificate parses");
        assert_eq!(before.unix(), params.not_before.unix_timestamp());
        assert_eq!(after.unix(), params.not_after.unix_timestamp());
        assert_eq!(
            before.to_string(),
            format!("{y1:04}-{m1:02}-{d1:02} 00:00:00Z")
        );
        assert_eq!(
            after.to_string(),
            format!("{y2:04}-{m2:02}-{d2:02} 00:00:00Z")
        );
        assert_eq!(time_tags(&der, (before, after)), tags);
    }
}

#[test]
fn times_to_the_second_match_rcgen() {
    let mut params = params();
    // 2001-09-09T01:46:40Z and one second before 2038-01-19T03:14:08Z.
    params.not_before = std::time::SystemTime::UNIX_EPOCH
        .checked_add(std::time::Duration::from_secs(1_000_000_000))
        .unwrap()
        .into();
    params.not_after = std::time::SystemTime::UNIX_EPOCH
        .checked_add(std::time::Duration::from_secs(2_147_483_647))
        .unwrap()
        .into();
    let (before, after) = validity(&certificate(&params)).unwrap();
    assert_eq!(before.unix(), 1_000_000_000);
    assert_eq!(before.to_string(), "2001-09-09 01:46:40Z");
    assert_eq!(after.unix(), 2_147_483_647);
    assert_eq!(after.to_string(), "2038-01-19 03:14:07Z");
}

#[test]
fn time_parsing_is_strict() {
    let utc = |text: &str| Time::parse(UTC_TIME, text.as_bytes());
    let general = |text: &str| Time::parse(GENERALIZED_TIME, text.as_bytes());
    // The UTCTime century pivot.
    assert_eq!(
        utc("500101000000Z").unwrap().to_string(),
        "1950-01-01 00:00:00Z"
    );
    assert_eq!(
        utc("491231235959Z").unwrap().to_string(),
        "2049-12-31 23:59:59Z"
    );
    assert_eq!(utc("700101000000Z").unwrap().unix(), 0);
    assert_eq!(utc("691231235959Z").unwrap().unix(), -1);
    assert_eq!(utc("491231235959Z").unwrap().unix(), 2_524_607_999);
    // GeneralizedTime before 2050 is not what RFC 5280 asks for, but it is
    // unambiguous and other tools emit it.
    assert_eq!(general("19700101000000Z").unwrap().unix(), 0);
    assert_eq!(general("19691231235959Z").unwrap().unix(), -1);
    assert_eq!(general("20261127120000Z").unwrap().unix(), 1_795_780_800);
    assert_eq!(general("20000229000000Z").unwrap().unix(), 951_782_400);
    assert_eq!(general("99991231235959Z").unwrap().unix(), 253_402_300_799);
    for bad in [
        "",
        "Z",
        "7001010000Z",       // no seconds
        "700101000000",      // no Z
        "700101000000+0000", // an offset
        "7001010000.0Z",
        "701301000000Z", // month 13
        "700001000000Z", // month 0
        "700100000000Z", // day 0
        "700132000000Z",
        "700229000000Z", // 1970 is not a leap year
        "700101240000Z",
        "700101006000Z",
        "700101000060Z", // no leap seconds
        "70010100000 Z",
        "-00101000000Z",
        "19700101000000Z", // GeneralizedTime text under the UTCTime tag
    ] {
        assert_eq!(utc(bad), None, "{bad:?}");
    }
    for bad in [
        "700101000000Z", // UTCTime text under the GeneralizedTime tag
        "19700101000000",
        "19700101000000.5Z",
        "197001010000Z",
        "21000229000000Z", // 2100 is not a leap year
        "19700101000000+0100",
    ] {
        assert_eq!(general(bad), None, "{bad:?}");
    }
    // Any other tag is not a time.
    assert_eq!(Time::parse(0x0c, b"700101000000Z"), None);
}

/// Wraps `contents` as one DER element.
fn element(tag: u8, contents: &[u8]) -> Vec<u8> {
    let mut out = vec![tag];
    match contents.len() {
        n if n < 0x80 => out.push(n as u8),
        n if n < 0x100 => out.extend([0x81, n as u8]),
        n => out.extend([0x82, (n >> 8) as u8, n as u8]),
    }
    out.extend(contents);
    out
}

/// A certificate skeleton with the given TBSCertificate fields.
fn skeleton(tbs: &[Vec<u8>]) -> Vec<u8> {
    let tbs = element(SEQUENCE, &tbs.concat());
    let algorithm = element(SEQUENCE, &[]);
    let signature = element(BIT_STRING, &[0]);
    element(SEQUENCE, &[tbs, algorithm, signature].concat())
}

fn fields(version: bool, not_before: Vec<u8>, not_after: Vec<u8>) -> Vec<Vec<u8>> {
    let mut tbs = vec![];
    if version {
        tbs.push(element(VERSION, &element(INTEGER, &[2])));
    }
    tbs.push(element(INTEGER, &[1]));
    tbs.push(element(SEQUENCE, &[]));
    tbs.push(element(SEQUENCE, &[]));
    tbs.push(element(SEQUENCE, &[not_before, not_after].concat()));
    // subject and subjectPublicKeyInfo follow; the walk stops before them.
    tbs.push(element(SEQUENCE, &[]));
    tbs
}

#[test]
fn the_walk_accepts_only_the_expected_structure() {
    let utc = element(UTC_TIME, b"240101000000Z");
    let general = element(GENERALIZED_TIME, b"20600101000000Z");
    // A v1 certificate has no version field.
    for version in [true, false] {
        let der = skeleton(&fields(version, utc.clone(), general.clone()));
        let (before, after) = validity(&der).unwrap();
        assert_eq!(before.to_string(), "2024-01-01 00:00:00Z");
        assert_eq!(after.to_string(), "2060-01-01 00:00:00Z");
    }
    let good = skeleton(&fields(true, utc.clone(), general.clone()));
    // Bytes after the certificate.
    let mut trailing = good.clone();
    trailing.push(0);
    assert_eq!(validity(&trailing), None);
    assert!(!is_certificate(&trailing));
    // Every proper prefix.
    for end in 0..good.len() {
        assert_eq!(validity(&good[..end]), None, "prefix {end}");
        assert!(!is_certificate(&good[..end]), "prefix {end}");
    }
    // A third element inside validity, and a missing notAfter.
    let mut tbs = fields(true, utc.clone(), general.clone());
    tbs[4] = element(
        SEQUENCE,
        &[utc.clone(), general.clone(), utc.clone()].concat(),
    );
    assert_eq!(validity(&skeleton(&tbs)), None);
    tbs[4] = element(SEQUENCE, &utc);
    assert_eq!(validity(&skeleton(&tbs)), None);
    // Trailing bytes inside the version wrapper; a non-integer version.
    tbs = fields(true, utc.clone(), general.clone());
    tbs[0] = element(VERSION, &[element(INTEGER, &[2]), vec![5, 0]].concat());
    assert_eq!(validity(&skeleton(&tbs)), None);
    tbs[0] = element(VERSION, &element(BIT_STRING, &[2]));
    assert_eq!(validity(&skeleton(&tbs)), None);
    // A field of the wrong type in each position before validity.
    for i in 1..5 {
        let mut tbs = fields(true, utc.clone(), general.clone());
        tbs[i] = element(BIT_STRING, &[0]);
        assert_eq!(validity(&skeleton(&tbs)), None, "field {i}");
    }
    // The outer shape: a fourth element, a missing signature.
    let tbs = element(
        SEQUENCE,
        &fields(true, utc.clone(), general.clone()).concat(),
    );
    let algorithm = element(SEQUENCE, &[]);
    let signature = element(BIT_STRING, &[0]);
    let four = [tbs.clone(), algorithm.clone(), signature.clone(), signature].concat();
    assert!(!is_certificate(&element(SEQUENCE, &four)));
    assert!(!is_certificate(&element(
        SEQUENCE,
        &[tbs, algorithm].concat()
    )));
}

#[test]
fn lengths_must_be_definite_and_minimal() {
    let contents = [INTEGER, 1, 7];
    let read = |bytes: &[u8]| {
        let mut der = Der(bytes);
        let found = der.next()?;
        der.end()?;
        Some(found.1.to_vec())
    };
    assert_eq!(read(&[SEQUENCE, 3, INTEGER, 1, 7]), Some(contents.to_vec()));
    // Long form for a length under 128, and a leading zero length byte.
    assert_eq!(read(&[SEQUENCE, 0x81, 3, INTEGER, 1, 7]), None);
    assert_eq!(read(&[SEQUENCE, 0x82, 0, 3, INTEGER, 1, 7]), None);
    // Indefinite length (BER) with an end-of-contents marker.
    assert_eq!(read(&[SEQUENCE, 0x80, INTEGER, 1, 7, 0, 0]), None);
    // A length past the end, a huge length, more than four length bytes.
    assert_eq!(read(&[SEQUENCE, 4, INTEGER, 1, 7]), None);
    assert_eq!(read(&[SEQUENCE, 0x84, 0xff, 0xff, 0xff, 0xff, 1]), None);
    assert_eq!(read(&[SEQUENCE, 0x85, 1, 0, 0, 0, 0]), None);
    assert_eq!(
        read(&[
            SEQUENCE, 0x88, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff
        ]),
        None
    );
    // A high tag number.
    assert_eq!(read(&[0x3f, 0x10, 0]), None);
    // A proper long form.
    let long = element(SEQUENCE, &[0; 200]);
    assert_eq!(long[..3], [SEQUENCE, 0x81, 200]);
    assert_eq!(read(&long), Some(vec![0; 200]));
}

proptest! {
    #[test]
    fn arbitrary_bytes_never_panic(bytes in proptest::collection::vec(any::<u8>(), 0..2048)) {
        let _ = validity(&bytes);
        let _ = is_certificate(&bytes);
    }

    #[test]
    fn arbitrary_time_text_never_panics(tag in any::<u8>(), text in proptest::collection::vec(any::<u8>(), 0..20)) {
        if let Some(time) = Time::parse(tag, &text) {
            let _ = time.unix();
            let _ = time.to_string();
        }
    }

    // Byte flips, truncations and insertions of a real certificate: the walk
    // answers or refuses, and what it answers still formats and converts.
    #[test]
    fn mutated_certificates_never_panic(
        edits in proptest::collection::vec((any::<prop::sample::Index>(), any::<u8>(), 0..3_u8), 1..8),
        cut in proptest::option::of(any::<prop::sample::Index>()),
    ) {
        thread_local! {
            static VALID: Vec<u8> = certificate(&params());
        }
        let mut der = VALID.with(Clone::clone);
        for (at, byte, kind) in edits {
            let at = at.index(der.len());
            match kind {
                0 => der[at] = byte,
                1 => der.insert(at, byte),
                _ => { der.remove(at); }
            }
        }
        if let Some(cut) = cut {
            der.truncate(cut.index(der.len() + 1));
        }
        if let Some((before, after)) = validity(&der) {
            prop_assert!(is_certificate(&der));
            let _ = (before.unix(), after.unix(), before.to_string(), after.to_string());
        }
        let _ = is_certificate(&der);
    }
}
