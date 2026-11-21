//! Item 13 edge cases: header validation in `read_log`, invalid schemas at
//! create time, and string values with embedded NUL bytes.
//!
//! Pinned from the documented behaviour in `src/log.rs`: a file whose bytes
//! disagree with the magic (comparing the bytes present) is `BadMagic`; a
//! file that is a strict prefix of the magic or of the 12-byte header is a
//! truncated header, `Corrupt`.

use kala_core::log::{EntryBody, LOG_FORMAT_VERSION, LOG_MAGIC, read_log};
use kala_core::*;

fn write_file(bytes: &[u8]) -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("f.kala");
    std::fs::write(&p, bytes).unwrap();
    (dir, p)
}

type ErrCheck = fn(&Error) -> bool;

fn header(version: u32) -> Vec<u8> {
    let mut h = LOG_MAGIC.to_vec();
    h.extend_from_slice(&version.to_le_bytes());
    h
}

#[test]
fn short_file_with_wrong_bytes_is_bad_magic() {
    let (_d, p) = write_file(b"xx");
    assert_eq!(read_log(&p), Err(Error::BadMagic));
}

#[test]
fn full_length_wrong_magic_is_bad_magic() {
    let mut h = header(LOG_FORMAT_VERSION);
    h[0] = b'X';
    let (_d, p) = write_file(&h);
    assert_eq!(read_log(&p), Err(Error::BadMagic));
}

#[test]
fn seven_byte_magic_prefix_is_a_truncated_header() {
    let (_d, p) = write_file(b"KALALOG");
    assert!(matches!(read_log(&p), Err(Error::Corrupt(_))));
}

#[test]
fn magic_without_full_version_is_a_truncated_header() {
    let mut h = LOG_MAGIC.to_vec();
    h.extend_from_slice(&[1, 0]);
    let (_d, p) = write_file(&h);
    assert!(matches!(read_log(&p), Err(Error::Corrupt(_))));
}

#[test]
fn empty_file_is_not_a_valid_log() {
    let (_d, p) = write_file(b"");
    assert!(matches!(
        read_log(&p),
        Err(Error::Corrupt(_)) | Err(Error::BadMagic)
    ));
}

#[test]
fn version_two_is_unsupported() {
    let (_d, p) = write_file(&header(2));
    assert_eq!(read_log(&p), Err(Error::UnsupportedVersion(2)));
}

#[test]
fn header_only_file_has_no_entries() {
    let (_d, p) = write_file(&header(LOG_FORMAT_VERSION));
    assert_eq!(read_log(&p), Ok(vec![]));
}

#[test]
fn missing_file_is_io_not_found() {
    let dir = tempfile::tempdir().unwrap();
    let e = read_log(&dir.path().join("nope")).unwrap_err();
    assert!(matches!(
        e,
        Error::Io {
            kind: std::io::ErrorKind::NotFound,
            ..
        }
    ));
}

#[test]
fn create_with_duplicate_or_empty_names_creates_no_file() {
    let cases: Vec<(Schema, ErrCheck)> = vec![
        (Schema::new().node_type("A").node_type("A"), |e| {
            matches!(e, Error::DuplicateName(_))
        }),
        (
            Schema::new()
                .attribute(AttrDef::new(
                    "x",
                    ValueType::I64,
                    Kind::State,
                    Cardinality::One,
                ))
                .attribute(AttrDef::new(
                    "x",
                    ValueType::I64,
                    Kind::State,
                    Cardinality::One,
                )),
            |e| matches!(e, Error::DuplicateName(_)),
        ),
        (Schema::new().node_type(""), |e| {
            matches!(e, Error::InvalidName(_))
        }),
        (
            Schema::new().attribute(AttrDef::new(
                "",
                ValueType::I64,
                Kind::State,
                Cardinality::One,
            )),
            |e| matches!(e, Error::InvalidName(_)),
        ),
    ];
    for (i, (schema, check)) in cases.into_iter().enumerate() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("w.kala");
        let err = World::create(&p, schema).expect_err("invalid schema");
        assert!(check(&err), "case {i}: {err:?}");
        assert!(!p.exists(), "case {i}: file must not exist");
        assert_eq!(
            std::fs::read_dir(dir.path()).unwrap().count(),
            0,
            "case {i}"
        );
    }
}

#[test]
fn str_value_with_embedded_nul_round_trips_through_the_log() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("w.kala");
    let mut w = World::create(
        &p,
        Schema::new().node_type("C").attribute(AttrDef::new(
            "name",
            ValueType::Str,
            Kind::State,
            Cardinality::Many,
        )),
    )
    .unwrap();
    let c = w.create_entity("C", None).unwrap();
    for (i, s) in ["a\0b", "\0", "\0\0", "tail\0", "\0\u{ff}\0"]
        .iter()
        .enumerate()
    {
        w.assert(c, "name", Value::Str((*s).into()), Some(Tick(i as i64)))
            .unwrap();
    }
    let got: Vec<String> = read_log(&p)
        .unwrap()
        .into_iter()
        .filter_map(|e| match e.body {
            EntryBody::Fact {
                value: Value::Str(s),
                ..
            } => Some(s),
            _ => None,
        })
        .collect();
    assert_eq!(got, vec!["a\0b", "\0", "\0\0", "tail\0", "\0\u{ff}\0"]);
}
