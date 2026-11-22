//! Log format v2: per-transaction framing (docs/slice-2-decisions.md, Item 1).
//!
//! Every rule of the recovery / scan classification list has a test here.
//! All bytes are built from the documented layout (see `tests/common`), never
//! by calling private code.
//!
//! ```text
//! file  = header(12) txn*        txn  = begin(25) body
//! begin = 0x07 | tx u64 | body_len u64 | body_crc u32 | begin_crc u32   (LE)
//! ```
//!
//! Classification (applied txn by txn from offset 12, expected_tx from 0):
//!  1 no bytes left -> clean end        2 < 25 bytes left -> torn
//!  3 begin_crc bad -> torn iff rest of file is all 0x00, else Corrupt
//!  4 tag != 7 or tx != expected -> Corrupt
//!  5 body_len > bytes left -> torn     6 body_crc bad -> Corrupt (never cut)
//!  7 body content invalid -> Corrupt   8 otherwise committed

mod common;

use common::{BEGIN_LEN, BEGIN_TAG, HEADER_LEN};
use kala_core::log::{LOG_FORMAT_VERSION, LogEntry, read_log};
use kala_core::*;
use std::path::{Path, PathBuf};

fn schema() -> Schema {
    Schema::new()
        .node_type("CHARACTER")
        .node_type("PLACE")
        .attribute(AttrDef::new(
            "located_in",
            ValueType::Ref,
            Kind::State,
            Cardinality::One,
        ))
        .attribute(AttrDef::new(
            "name",
            ValueType::Str,
            Kind::State,
            Cardinality::One,
        ))
        .attribute(AttrDef::new(
            "age",
            ValueType::I64,
            Kind::State,
            Cardinality::Many,
        ))
}

fn t(x: i64) -> Option<Tick> {
    Some(Tick(x))
}

/// genesis (tx0) + 4 more txns; latest_tx == 4. Returns the world's ids too.
fn build() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("world.kala");
    let mut w = World::create(&path, schema()).unwrap();
    let greg = w.create_entity("CHARACTER", Some("greg")).unwrap(); // tx1
    let varrow = w.create_entity("PLACE", Some("varrow")).unwrap(); // tx2
    w.assert(greg, "located_in", Value::Ref(varrow), t(-300))
        .unwrap(); // tx3
    w.assert(greg, "name", Value::Str("Greg".into()), t(0))
        .unwrap(); // tx4
    assert_eq!(w.latest_tx(), TxId(4));
    drop(w);
    (dir, path)
}

fn bytes(p: &Path) -> Vec<u8> {
    std::fs::read(p).unwrap()
}

/// Indirection so the recovery report is read in one place.
fn bytes_cut(w: &World) -> u64 {
    w.recovery().bytes_cut
}
fn quarantine_of(w: &World) -> Option<PathBuf> {
    w.recovery().quarantine.clone()
}
fn report_is_default(w: &World) -> bool {
    *w.recovery() == RecoveryReport::default()
}

fn quarantine_files(path: &Path) -> Vec<PathBuf> {
    let prefix = format!("{}.torn-", path.file_name().unwrap().to_string_lossy());
    let mut v: Vec<PathBuf> = std::fs::read_dir(path.parent().unwrap())
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| {
            p.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with(&prefix)
        })
        .collect();
    v.sort();
    v
}

fn qpath(path: &Path, suffix: &str) -> PathBuf {
    let mut s = path.as_os_str().to_owned();
    s.push(suffix);
    PathBuf::from(s)
}

fn open_err(p: &Path) -> Error {
    match World::open(p) {
        Err(e) => e,
        Ok(_) => panic!("open unexpectedly succeeded"),
    }
}

/// Writes `file` to `path`, then asserts open AND read_log refuse with
/// Corrupt, the file is byte-identical and no quarantine file appeared.
fn assert_refused(path: &Path, file: &[u8], label: &str) {
    std::fs::write(path, file).unwrap();
    match World::open(path) {
        Err(Error::Corrupt(_)) => {}
        Err(e) => panic!("{label}: open: expected Corrupt, got {e:?}"),
        Ok(_) => panic!("{label}: open: expected Corrupt, open succeeded"),
    }
    assert_eq!(bytes(path), file, "{label}: open modified the file");
    match read_log(path) {
        Err(Error::Corrupt(_)) => {}
        Err(e) => panic!("{label}: read_log: expected Corrupt, got {e:?}"),
        Ok(v) => panic!(
            "{label}: read_log: expected Corrupt, got {} entries",
            v.len()
        ),
    }
    assert_eq!(bytes(path), file, "{label}: read_log modified the file");
    assert!(
        quarantine_files(path).is_empty(),
        "{label}: quarantine made"
    );
}

/// `valid` is a committed log, `tail` torn bytes after it. open() must cut the
/// tail, quarantine it and report it; read_log must ignore it, not touching
/// the file.
fn assert_torn(path: &Path, valid: &[u8], tail: &[u8], label: &str) {
    let mut file = valid.to_vec();
    file.extend_from_slice(tail);
    std::fs::write(path, valid).unwrap();
    let committed: Vec<LogEntry> = read_log(path).unwrap();
    std::fs::write(path, &file).unwrap();

    assert_eq!(read_log(path).unwrap(), committed, "{label}: read_log");
    assert_eq!(bytes(path), file, "{label}: read_log modified the file");
    assert!(
        quarantine_files(path).is_empty(),
        "{label}: read_log quarantined"
    );

    let w = match World::open(path) {
        Ok(w) => w,
        Err(e) => panic!("{label}: torn tail must open, got {e:?}"),
    };
    let q = qpath(path, &format!(".torn-{}", valid.len()));
    assert_eq!(bytes_cut(&w), tail.len() as u64, "{label}: bytes_cut");
    assert_eq!(
        quarantine_of(&w),
        Some(q.clone()),
        "{label}: quarantine path"
    );
    drop(w);
    assert_eq!(
        bytes(path),
        valid,
        "{label}: file must be cut to the valid prefix"
    );
    assert_eq!(
        bytes(&q),
        tail,
        "{label}: quarantine holds exactly the cut bytes"
    );
    assert_eq!(quarantine_files(path), vec![q.clone()], "{label}");
    // the cut is final: a second open is clean
    let w = match World::open(path) {
        Ok(w) => w,
        Err(e) => panic!("{label}: second open: {e:?}"),
    };
    assert!(report_is_default(&w), "{label}: second open is clean");
    drop(w);
    std::fs::remove_file(&q).unwrap();
}

/// Slice 3 item 0a: `file` is the clean log `orig` with the LAST txn damaged
/// (valid begin record, body_crc mismatch, nothing valid after it). open() must
/// cut it back to `keep` (the end of the previous txn), quarantine the cut
/// bytes and report `Damaged`; read_log ignores it, the file stays untouched.
fn assert_damaged_cut(path: &Path, orig: &[u8], file: &[u8], keep: usize, label: &str) {
    std::fs::write(path, &orig[..keep]).unwrap();
    let committed: Vec<LogEntry> = read_log(path).unwrap();
    std::fs::write(path, file).unwrap();
    assert_eq!(read_log(path).unwrap(), committed, "{label}: read_log");
    assert_eq!(bytes(path), file, "{label}: read_log modified the file");
    assert!(
        quarantine_files(path).is_empty(),
        "{label}: read_log quarantined"
    );
    let w = match World::open(path) {
        Ok(w) => w,
        Err(e) => panic!("{label}: expected a cut, got {e:?}"),
    };
    assert_eq!(
        w.recovery().cause,
        Some(CutCause::Damaged),
        "{label}: cause"
    );
    assert_eq!(
        bytes_cut(&w),
        (file.len() - keep) as u64,
        "{label}: bytes_cut"
    );
    let q = qpath(path, &format!(".torn-{keep}"));
    assert_eq!(
        quarantine_of(&w),
        Some(q.clone()),
        "{label}: quarantine path"
    );
    assert_eq!(w.latest_tx(), TxId(3), "{label}: last txn is gone");
    drop(w);
    assert_eq!(bytes(path), &file[..keep], "{label}: cut file");
    assert_eq!(
        bytes(&q),
        &file[keep..],
        "{label}: quarantine holds the cut bytes"
    );
    let w = World::open(path).unwrap_or_else(|e| panic!("{label}: second open {e:?}"));
    assert!(report_is_default(&w), "{label}: second open is clean");
    drop(w);
    std::fs::remove_file(&q).unwrap();
}

/// (tx, entry id of the first frame, frames before the Commit, Commit entry id)
/// of the LAST txn of a well-formed file.
fn last_parts(file: &[u8]) -> (u64, u64, Vec<u8>, u64) {
    let s = *common::spans(file).last().unwrap();
    let fr = common::frame_spans(file, s);
    let first = u64::from_le_bytes(file[fr[0].0 + 5..fr[0].0 + 13].try_into().unwrap());
    let c = fr.last().unwrap();
    assert_eq!(c.2, 5, "last frame is the Commit");
    let cid = u64::from_le_bytes(file[c.0 + 5..c.0 + 13].try_into().unwrap());
    (s.tx, first, file[s.body_start..c.0].to_vec(), cid)
}

/// The file with the last txn's begin+body replaced by `txn(tx, body)`.
fn with_last_body(file: &[u8], tx: u64, body: &[u8]) -> Vec<u8> {
    let s = *common::spans(file).last().unwrap();
    let mut out = file[..s.start].to_vec();
    out.extend_from_slice(&common::txn(tx, body));
    out
}

// ---------------------------------------------------------------------------
// Layout
// ---------------------------------------------------------------------------

#[test]
fn every_txn_starts_with_a_documented_begin_record() {
    let (_d, path) = build();
    let b = bytes(&path);
    assert_eq!(&b[..8], b"KALALOG\0");
    assert_eq!(&b[8..12], &3u32.to_le_bytes());
    assert_eq!(LOG_FORMAT_VERSION, 3);
    let spans = common::spans(&b);
    assert_eq!(spans.len(), 5, "genesis + 4 txns");
    assert_eq!(
        spans[0].start, HEADER_LEN,
        "begin follows the header directly"
    );
    for (i, s) in spans.iter().enumerate() {
        assert_eq!(s.tx, i as u64, "tx numbers are consecutive from 0");
        assert_eq!(b[s.start], BEGIN_TAG);
        let body = &b[s.body_start..s.end];
        let body_crc = u32::from_le_bytes(b[s.start + 17..s.start + 21].try_into().unwrap());
        let begin_crc = u32::from_le_bytes(b[s.start + 21..s.start + 25].try_into().unwrap());
        assert_eq!(body_crc, common::crc(body), "tx {i}: body_crc");
        assert_eq!(
            begin_crc,
            common::crc(&b[s.start..s.start + 21]),
            "tx {i}: begin_crc"
        );
        // last frame is the Commit; its checksum does NOT cover the begin record
        let fr = common::frame_spans(&b, *s);
        let (cs, ce, kind) = *fr.last().unwrap();
        assert_eq!(kind, 5);
        let commit_tx = u64::from_le_bytes(b[cs + 13..cs + 21].try_into().unwrap());
        assert_eq!(commit_tx, s.tx, "tx {i}: Commit tx == begin tx");
        let stored = u32::from_le_bytes(b[ce - 4..ce].try_into().unwrap());
        assert_eq!(
            stored,
            common::crc(&b[s.body_start..ce - 4]),
            "tx {i}: commit checksum"
        );
        assert_eq!(
            fr.iter().filter(|f| f.2 == 5).count(),
            1,
            "one Commit per txn"
        );
    }
    assert_eq!(
        spans.last().unwrap().end,
        b.len(),
        "no bytes after the last txn"
    );
}

// ---------------------------------------------------------------------------
// v1 files
// ---------------------------------------------------------------------------

#[test]
fn v1_header_is_unsupported_version_1_for_open_and_read_log() {
    let dir = tempfile::tempdir().unwrap();
    // header-only, and header + something that looks like v1 frames
    let mut with_frames = common::header(1);
    with_frames.extend_from_slice(&common::frame(5, 1, &[0; 12]));
    for (i, file) in [common::header(1), with_frames].into_iter().enumerate() {
        let p = dir.path().join(format!("v1-{i}.kala"));
        std::fs::write(&p, &file).unwrap();
        assert_eq!(open_err(&p), Error::UnsupportedVersion(1), "case {i}");
        assert_eq!(read_log(&p), Err(Error::UnsupportedVersion(1)), "case {i}");
        assert_eq!(bytes(&p), file, "case {i}: file untouched");
        assert!(quarantine_files(&p).is_empty());
    }
}

#[test]
fn a_real_world_with_its_version_rewritten_to_1_is_unsupported() {
    let (_d, path) = build();
    let mut b = bytes(&path);
    b[8..12].copy_from_slice(&1u32.to_le_bytes());
    std::fs::write(&path, &b).unwrap();
    assert_eq!(open_err(&path), Error::UnsupportedVersion(1));
    assert_eq!(read_log(&path), Err(Error::UnsupportedVersion(1)));
    assert_eq!(bytes(&path), b);
}

#[test]
fn v2_header_is_unsupported_version_2_for_open_and_read_log() {
    let dir = tempfile::tempdir().unwrap();
    let mut with_txn = common::header(2);
    with_txn.extend_from_slice(&common::txn(0, &[]));
    for (i, file) in [common::header(2), with_txn].into_iter().enumerate() {
        let p = dir.path().join(format!("v2-{i}.kala"));
        std::fs::write(&p, &file).unwrap();
        assert_eq!(open_err(&p), Error::UnsupportedVersion(2), "case {i}");
        assert_eq!(read_log(&p), Err(Error::UnsupportedVersion(2)), "case {i}");
        assert_eq!(bytes(&p), file, "case {i}: file untouched");
        assert!(quarantine_files(&p).is_empty());
    }
}

#[test]
fn a_real_world_with_its_version_rewritten_to_2_is_unsupported() {
    let (_d, path) = build();
    let mut b = bytes(&path);
    b[8..12].copy_from_slice(&2u32.to_le_bytes());
    std::fs::write(&path, &b).unwrap();
    assert_eq!(open_err(&path), Error::UnsupportedVersion(2));
    assert_eq!(read_log(&path), Err(Error::UnsupportedVersion(2)));
    assert_eq!(bytes(&path), b);
}

// ---------------------------------------------------------------------------
// Rule 1: clean end; recovery report defaults
// ---------------------------------------------------------------------------

#[test]
fn clean_open_reports_the_default_recovery() {
    let (_d, path) = build();
    let before = bytes(&path);
    let w = World::open(&path).unwrap();
    assert_eq!(*w.recovery(), RecoveryReport::default());
    assert_eq!(w.recovery().bytes_cut, 0);
    assert_eq!(w.recovery().quarantine, None);
    drop(w);
    assert_eq!(bytes(&path), before);
    assert!(quarantine_files(&path).is_empty());
}

#[test]
fn create_and_in_memory_report_the_default_recovery() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("w.kala");
    let w = World::create(&p, schema()).unwrap();
    assert_eq!(*w.recovery(), RecoveryReport::default());
    drop(w);
    let w = World::in_memory(schema()).unwrap();
    assert_eq!(*w.recovery(), RecoveryReport::default());
}

#[test]
fn recovery_report_type_is_debug_clone_eq_default() {
    let r = RecoveryReport {
        bytes_cut: 3,
        quarantine: Some(PathBuf::from("x.torn-1")),
        cause: Some(CutCause::Torn),
    };
    let c = r.clone();
    assert_eq!(r, c);
    assert_ne!(r, RecoveryReport::default());
    assert_eq!(RecoveryReport::default().cause, None);
    let d = RecoveryReport {
        cause: Some(CutCause::Damaged),
        ..r.clone()
    };
    assert_ne!(r, d);
    assert!(!format!("{r:?}").is_empty());
}

#[test]
fn header_only_v2_file_has_no_genesis_and_is_corrupt_untouched_for_open() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("w.kala");
    // header only, header + torn begin: neither has a committed genesis
    for (i, extra) in [vec![], vec![BEGIN_TAG, 0, 0], vec![0u8; 40]]
        .into_iter()
        .enumerate()
    {
        let mut file = common::header(kala_core::log::LOG_FORMAT_VERSION);
        file.extend_from_slice(&extra);
        std::fs::write(&p, &file).unwrap();
        assert!(matches!(open_err(&p), Error::Corrupt(_)), "case {i}");
        assert_eq!(bytes(&p), file, "case {i}: untouched");
        assert!(quarantine_files(&p).is_empty(), "case {i}");
    }
}

// ---------------------------------------------------------------------------
// Rule 2: fewer than 25 bytes left -> torn
// ---------------------------------------------------------------------------

#[test]
fn torn_begin_fewer_than_25_bytes_is_cut_quarantined_and_reported() {
    let (_d, path) = build();
    let valid = bytes(&path);
    let begin = common::begin(5, &common::commit_frame(&[], 99, 5));
    for n in 1..BEGIN_LEN {
        assert_torn(
            &path,
            &valid,
            &begin[..n],
            &format!("begin prefix of {n} bytes"),
        );
    }
    // non-begin junk of the same sizes
    for n in [1usize, 7, 24] {
        assert_torn(&path, &valid, &vec![0xC3; n], &format!("junk of {n} bytes"));
    }
}

// ---------------------------------------------------------------------------
// Rule 5: body_len past EOF -> torn
// ---------------------------------------------------------------------------

#[test]
fn body_len_past_eof_is_torn_cut_quarantined_and_reported() {
    let (_d, path) = build();
    let valid = bytes(&path);
    let next_tx = 5u64;
    for (len, have) in [(100u64, 0usize), (100, 1), (100, 50), (100, 99), (1, 0)] {
        let mut tail = common::begin_raw(BEGIN_TAG, next_tx, len, 0xDEAD_BEEF);
        tail.extend(std::iter::repeat_n(0xAB, have));
        assert_torn(
            &path,
            &valid,
            &tail,
            &format!("len {len}, {have} body bytes present"),
        );
    }
    for len in [u64::MAX, u64::MAX - 24, i64::MAX as u64, 1 << 40] {
        let mut tail = common::begin_raw(BEGIN_TAG, next_tx, len, 0);
        tail.extend_from_slice(&[1, 2, 3]);
        assert_torn(&path, &valid, &tail, &format!("huge len {len}"));
    }
}

#[test]
fn a_real_last_txn_cut_at_every_byte_is_torn() {
    let (_d, path) = build();
    let full = bytes(&path);
    let last = *common::spans(&full).last().unwrap();
    let valid = full[..last.start].to_vec();
    for k in last.start + 1..last.end {
        assert_torn(&path, &valid, &full[last.start..k], &format!("cut at {k}"));
    }
}

#[test]
fn torn_last_txn_leaves_writes_continuing_with_a_well_formed_next_txn() {
    let (_d, path) = build();
    let full = bytes(&path);
    let last = *common::spans(&full).last().unwrap();
    std::fs::write(&path, &full[..last.end - 3]).unwrap();
    let mut w = World::open(&path).unwrap();
    assert_eq!(w.latest_tx(), TxId(3));
    assert_eq!(bytes_cut(&w), (last.end - 3 - last.start) as u64);
    let greg = w.entity_by_handle("greg").unwrap();
    w.assert(greg, "age", Value::I64(41), t(7)).unwrap();
    drop(w);
    let b = bytes(&path);
    let spans = common::spans(&b);
    assert_eq!(spans.len(), 5);
    assert_eq!(
        spans.last().unwrap().tx,
        4,
        "the cut txn's number is reused"
    );
    assert_eq!(spans.last().unwrap().end, b.len());
    let w = World::open(&path).unwrap();
    assert!(report_is_default(&w));
    assert!(
        w.as_of_value(greg, "age", &Value::I64(41), Tick(7))
            .unwrap()
    );
}

// ---------------------------------------------------------------------------
// Rule 3: begin_crc mismatch -> torn iff the rest is all zeros, else Corrupt
// ---------------------------------------------------------------------------

#[test]
fn all_zero_tail_is_torn() {
    let (_d, path) = build();
    let valid = bytes(&path);
    for n in [1usize, 24, 25, 26, 49, 50, 4096] {
        assert_torn(&path, &valid, &vec![0u8; n], &format!("{n} zero bytes"));
    }
}

#[test]
fn last_txn_overwritten_with_zeros_is_torn_not_corrupt() {
    // A zero-filled tail is how some filesystems expose an unflushed append.
    let (_d, path) = build();
    let full = bytes(&path);
    let last = *common::spans(&full).last().unwrap();
    let valid = full[..last.start].to_vec();
    assert_torn(
        &path,
        &valid,
        &vec![0u8; last.end - last.start],
        "zeroed last txn",
    );
}

#[test]
fn zero_tail_with_any_nonzero_byte_is_corrupt() {
    let (_d, path) = build();
    let valid = bytes(&path);
    for (n, at) in [
        (25usize, 0usize),
        (25, 24),
        (64, 40),
        (4096, 4095),
        (30, 29),
    ] {
        let mut tail = vec![0u8; n];
        tail[at] = 1;
        let mut file = valid.clone();
        file.extend_from_slice(&tail);
        assert_refused(&path, &file, &format!("{n} zeros, byte {at} nonzero"));
    }
}

#[test]
fn begin_crc_mismatch_with_nonzero_tail_is_corrupt_for_every_begin_byte() {
    let (_d, path) = build();
    let orig = bytes(&path);
    let spans = common::spans(&orig);
    // every byte of the begin record of the LAST txn, and of a middle txn
    for s in [*spans.last().unwrap(), spans[2]] {
        for i in 0..BEGIN_LEN {
            for mask in [0x01u8, 0x80, 0xFF] {
                let mut file = orig.clone();
                file[s.start + i] ^= mask;
                assert_refused(
                    &path,
                    &file,
                    &format!("tx {} begin byte {i} ^ {mask:#x}", s.tx),
                );
            }
        }
    }
}

#[test]
fn begin_crc_mismatch_in_the_first_txn_is_corrupt() {
    let (_d, path) = build();
    let mut file = bytes(&path);
    file[HEADER_LEN + 22] ^= 0x10;
    assert_refused(&path, &file, "genesis begin_crc");
}

// ---------------------------------------------------------------------------
// Rule 4: tag / tx number
// ---------------------------------------------------------------------------

#[test]
fn begin_with_a_valid_crc_but_wrong_tag_is_corrupt() {
    let (_d, path) = build();
    let orig = bytes(&path);
    let s = *common::spans(&orig).last().unwrap();
    let body = orig[s.body_start..s.end].to_vec();
    for tag in [0x00u8, 0x01, 0x05, 0x06, 0x08, 0xFF] {
        let mut file = orig[..s.start].to_vec();
        file.extend_from_slice(&common::begin_raw(
            tag,
            s.tx,
            body.len() as u64,
            common::crc(&body),
        ));
        file.extend_from_slice(&body);
        assert_refused(&path, &file, &format!("tag {tag:#x}"));
    }
}

#[test]
fn wrong_tag_with_body_len_past_eof_is_corrupt_not_torn() {
    // rule 4 comes before rule 5
    let (_d, path) = build();
    let mut file = bytes(&path);
    file.extend_from_slice(&common::begin_raw(0x08, 5, 1000, 0));
    assert_refused(&path, &file, "bad tag, long body");
}

#[test]
fn bad_tx_number_in_begin_is_corrupt() {
    let (_d, path) = build();
    let orig = bytes(&path);
    let (tx, _, _, _) = last_parts(&orig);
    let s = *common::spans(&orig).last().unwrap();
    let body = orig[s.body_start..s.end].to_vec();
    for bad in [tx + 1, tx + 100, tx - 1, 0, u64::MAX] {
        assert_refused(
            &path,
            &with_last_body(&orig, bad, &body),
            &format!("begin tx {bad}"),
        );
    }
}

#[test]
fn bad_tx_number_in_the_first_txn_is_corrupt() {
    let (_d, path) = build();
    let mut file = bytes(&path);
    let s = common::spans(&file)[0];
    let body = file[s.body_start..s.end].to_vec();
    let b = common::begin(1, &body); // genesis must be tx 0
    file[s.start..s.body_start].copy_from_slice(&b);
    assert_refused(&path, &file, "genesis begin says tx 1");
}

#[test]
fn torn_looking_begin_with_a_wrong_tx_is_corrupt_not_torn() {
    // a would-be torn tail whose tx number is wrong
    let (_d, path) = build();
    for tx in [4u64, 6, 99] {
        let mut file = bytes(&path);
        file.extend_from_slice(&common::begin_raw(BEGIN_TAG, tx, 5000, 0));
        file.extend_from_slice(&[7; 10]);
        assert_refused(&path, &file, &format!("torn-looking begin with tx {tx}"));
    }
}

// ---------------------------------------------------------------------------
// Rule 6: body_crc mismatch with all bytes present -> Corrupt (finding M1)
// ---------------------------------------------------------------------------

// Slice 3 item 0a (was `..._is_corrupt_and_untouched`, slice-1 finding M1):
// the last txn with all bytes present but a body_crc mismatch is cut as Damaged.
#[test]
fn flipping_any_byte_inside_the_last_committed_txn_body_is_cut_as_damaged() {
    let (_d, path) = build();
    let orig = bytes(&path);
    let s = *common::spans(&orig).last().unwrap();
    for i in s.body_start..s.end {
        let mut file = orig.clone();
        file[i] ^= 0x01;
        assert_damaged_cut(&path, &orig, &file, s.start, &format!("flip body byte {i}"));
    }
}

#[test]
fn flipping_any_byte_inside_a_middle_txn_body_is_corrupt_and_untouched() {
    let (_d, path) = build();
    let orig = bytes(&path);
    let s = common::spans(&orig)[2];
    for i in s.body_start..s.end {
        let mut file = orig.clone();
        file[i] ^= 0x80;
        assert_refused(&path, &file, &format!("flip body byte {i}"));
    }
}

// Slice 3 item 0a (was `flipping_the_last_commit_checksum_is_corrupt_not_cut`).
#[test]
fn flipping_the_last_commit_checksum_is_cut_as_damaged() {
    let (_d, path) = build();
    let orig = bytes(&path);
    let s = *common::spans(&orig).last().unwrap();
    for back in 1..=4 {
        let mut file = orig.clone();
        let n = file.len();
        file[n - back] ^= 0x01;
        assert_damaged_cut(
            &path,
            &orig,
            &file,
            s.start,
            &format!("commit checksum byte -{back}"),
        );
    }
}

// Slice 3 item 0a (was `begin_with_a_valid_crc_but_a_wrong_body_crc_is_corrupt`).
// A wrong body_crc under a valid begin record, last txn, nothing after: Damaged.
#[test]
fn begin_with_a_valid_crc_but_a_wrong_body_crc_in_the_last_txn_is_cut_as_damaged() {
    let (_d, path) = build();
    let orig = bytes(&path);
    let s = *common::spans(&orig).last().unwrap();
    let body = orig[s.body_start..s.end].to_vec();
    for bad in [common::crc(&body) ^ 1, 0, u32::MAX] {
        let mut file = orig[..s.start].to_vec();
        file.extend_from_slice(&common::begin_raw(BEGIN_TAG, s.tx, body.len() as u64, bad));
        file.extend_from_slice(&body);
        assert_damaged_cut(&path, &orig, &file, s.start, &format!("body_crc {bad:#x}"));
    }
}

// The Corrupt variant (kept intent): the same wrong body_crc in a MIDDLE txn,
// with valid txns after it, is never cut.
#[test]
fn begin_with_a_valid_crc_but_a_wrong_body_crc_in_a_middle_txn_is_corrupt() {
    let (_d, path) = build();
    let orig = bytes(&path);
    let s = common::spans(&orig)[2];
    let body = orig[s.body_start..s.end].to_vec();
    for bad in [common::crc(&body) ^ 1, 0, u32::MAX] {
        let mut file = orig[..s.start].to_vec();
        file.extend_from_slice(&common::begin_raw(BEGIN_TAG, s.tx, body.len() as u64, bad));
        file.extend_from_slice(&body);
        file.extend_from_slice(&orig[s.end..]);
        assert_refused(&path, &file, &format!("middle body_crc {bad:#x}"));
    }
}

#[test]
fn body_len_shorter_than_the_real_body_is_corrupt() {
    // the begin record is internally consistent for a shorter body (body_crc
    // matches the shorter slice) but the frames no longer fill it, and extra
    // bytes follow
    let (_d, path) = build();
    let orig = bytes(&path);
    let s = *common::spans(&orig).last().unwrap();
    let body = &orig[s.body_start..s.end];
    for cut in [1usize, 3, 4, 25] {
        let short = &body[..body.len() - cut];
        let mut file = orig[..s.start].to_vec();
        file.extend_from_slice(&common::begin(s.tx, short));
        file.extend_from_slice(body); // all original bytes still present
        assert_refused(&path, &file, &format!("body_len short by {cut}"));
    }
}

#[test]
fn body_len_longer_than_the_real_body_with_more_data_following_is_corrupt() {
    // two txns are claimed as one body: the frames of the first txn end at its
    // Commit, then more bytes follow inside the claimed body
    let (_d, path) = build();
    let orig = bytes(&path);
    let spans = common::spans(&orig);
    let (a, b) = (spans[3], spans[4]);
    let merged = orig[a.body_start..b.end].to_vec();
    let mut file = orig[..a.start].to_vec();
    file.extend_from_slice(&common::txn(a.tx, &merged));
    assert_refused(&path, &file, "two txns glued into one body");
}

// ---------------------------------------------------------------------------
// Rule 7: valid framing (both CRCs correct), invalid body content -> Corrupt
// ---------------------------------------------------------------------------

#[test]
fn body_content_invalid_with_valid_crcs_is_corrupt() {
    let (_d, path) = build();
    let orig = bytes(&path);
    let (tx, first, prior, cid) = last_parts(&orig);
    let commit = common::commit_frame(&prior, cid, tx);
    let good = [prior.clone(), commit.clone()].concat();
    // control: rebuilding the same txn is accepted (so failures below are
    // about the content, not about my reframing)
    assert_eq!(
        with_last_body(&orig, tx, &good),
        orig,
        "reframing is lossless"
    );

    let mut cases: Vec<(&str, Vec<u8>)> = vec![];
    cases.push(("empty body", vec![]));
    cases.push(("no Commit", prior.clone()));
    cases.push((
        "Commit tx != begin tx",
        [prior.clone(), common::commit_frame(&prior, cid, tx + 1)].concat(),
    ));
    cases.push((
        "Commit tx lower than begin tx",
        [prior.clone(), common::commit_frame(&prior, cid, tx - 1)].concat(),
    ));
    cases.push((
        "trailing bytes after Commit",
        [good.clone(), vec![0, 0, 0]].concat(),
    ));
    cases.push((
        "trailing zero byte after Commit",
        [good.clone(), vec![0]].concat(),
    ));
    cases.push((
        "second Commit after the first",
        [good.clone(), common::commit_frame(&good, cid + 1, tx)].concat(),
    ));
    cases.push((
        "Commit not last (a Fact frame after it)",
        [good.clone(), common::frame(4, cid + 1, &[0; 40])].concat(),
    ));
    cases.push(("Commit checksum mismatch", {
        let mut c = commit.clone();
        let n = c.len();
        c[n - 1] ^= 1;
        [prior.clone(), c].concat()
    }));
    cases.push(("Commit checksum computed over the begin record too", {
        let begin = common::begin(tx, &good);
        let mut over = begin.clone();
        over.extend_from_slice(&prior);
        let c = common::commit_frame(&over, cid, tx);
        [prior.clone(), c].concat()
    }));
    cases.push((
        "entry ID gap before the Commit",
        [prior.clone(), common::commit_frame(&prior, cid + 1, tx)].concat(),
    ));
    cases.push((
        "entry ID repeated at the Commit",
        [prior.clone(), common::commit_frame(&prior, cid - 1, tx)].concat(),
    ));
    cases.push(("Commit alone with the wrong entry ID", {
        [common::commit_frame(&[], first + 7, tx)].concat()
    }));
    cases.push(("whole txn shifted by one entry ID", {
        // first frame's entry ID bumped, checksum recomputed: sequence starts wrong
        let mut p = prior.clone();
        p[5..13].copy_from_slice(&(first + 1).to_le_bytes());
        [p.clone(), common::commit_frame(&p, cid, tx)].concat()
    }));
    cases.push(("unparseable entry (unknown kind)", {
        let f = common::frame(0xEE, first, &[1, 2, 3]);
        [f.clone(), common::commit_frame(&f, first + 1, tx)].concat()
    }));
    cases.push(("unparseable entry (truncated Fact body)", {
        let f = common::frame(4, first, &[1, 2, 3]);
        [f.clone(), common::commit_frame(&f, first + 1, tx)].concat()
    }));
    cases.push((
        "frame length runs past the body",
        [prior.clone(), commit[..commit.len() - 2].to_vec()].concat(),
    ));
    cases.push((
        "frame length prefix cut off at the end of the body",
        [good.clone(), vec![21, 0]].concat(),
    ));
    cases.push((
        "frame with a zero payload length",
        [good.clone(), vec![0, 0, 0, 0]].concat(),
    ));
    for (label, body) in cases {
        assert_refused(&path, &with_last_body(&orig, tx, &body), label);
    }
}

#[test]
fn body_content_invalid_in_the_first_txn_is_corrupt() {
    let (_d, path) = build();
    let orig = bytes(&path);
    let s = common::spans(&orig)[0];
    let fr = common::frame_spans(&orig, s);
    let prior = orig[s.body_start..fr.last().unwrap().0].to_vec();
    // genesis with the Commit tx changed to 1
    let bad = [
        prior.clone(),
        common::commit_frame(&prior, fr.len() as u64, 1),
    ]
    .concat();
    let mut file = orig[..s.start].to_vec();
    file.extend_from_slice(&common::txn(0, &bad));
    file.extend_from_slice(&orig[s.end..]);
    assert_refused(&path, &file, "genesis Commit says tx 1");
}

// ---------------------------------------------------------------------------
// read_log follows the same rules
// ---------------------------------------------------------------------------

#[test]
fn read_log_returns_the_committed_prefix_for_torn_tails_and_never_writes() {
    let (_d, path) = build();
    let valid = bytes(&path);
    let committed = read_log(&path).unwrap();
    let tails: Vec<Vec<u8>> = vec![
        vec![BEGIN_TAG],
        vec![0; 100],
        common::begin_raw(BEGIN_TAG, 5, 10_000, 0),
        [common::begin_raw(BEGIN_TAG, 5, 10_000, 0), vec![1; 50]].concat(),
    ];
    for (i, tail) in tails.iter().enumerate() {
        let mut file = valid.clone();
        file.extend_from_slice(tail);
        std::fs::write(&path, &file).unwrap();
        assert_eq!(read_log(&path).unwrap(), committed, "tail {i}");
        assert_eq!(bytes(&path), file, "tail {i}: read_log modified the file");
        assert!(quarantine_files(&path).is_empty(), "tail {i}");
    }
}

// ---------------------------------------------------------------------------
// Round trip
// ---------------------------------------------------------------------------

#[test]
fn round_trip_world_reads_back_the_same_and_read_log_has_no_begin_entries() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("w.kala");
    let mut w = World::create(&path, schema()).unwrap();
    let greg = w.create_entity("CHARACTER", Some("greg")).unwrap();
    let edwin = w.create_entity("CHARACTER", Some("edwin")).unwrap();
    let varrow = w.create_entity("PLACE", None).unwrap();
    w.assert(greg, "located_in", Value::Ref(varrow), t(-300))
        .unwrap();
    w.retract(greg, "located_in", Value::Ref(varrow), t(-250))
        .unwrap();
    let mut tx = w.begin();
    let lisa = tx.create_entity("CHARACTER", Some("lisa")).unwrap();
    tx.assert(lisa, "age", Value::I64(9), t(500)).unwrap();
    tx.assert(edwin, "name", Value::Str("Edwin".into()), t(i64::MIN))
        .unwrap();
    tx.assert(edwin, "age", Value::I64(1), t(i64::MAX)).unwrap();
    tx.commit().unwrap();
    let probes = [i64::MIN, -300, -251, -250, 0, 500, i64::MAX];
    let snap = |w: &World| -> String {
        let mut s = format!("{:?} {:?}\n", w.latest_tx(), w.latest_tick());
        for e in [greg, edwin, varrow, lisa] {
            for a in ["located_in", "name", "age"] {
                s += &format!("{e:?} {a} {:?}\n", w.history(e, a));
                for p in probes {
                    s += &format!("{e:?} {a} @{p} {:?}\n", w.as_of(e, a, Tick(p)));
                }
            }
        }
        s
    };
    let before = snap(&w);
    let tx_before = w.latest_tx();
    drop(w);

    let w = World::open(&path).unwrap();
    assert!(report_is_default(&w));
    assert_eq!(w.latest_tx(), tx_before);
    assert_eq!(snap(&w), before);
    assert_eq!(w.entity_by_handle("lisa"), Some(lisa));
    drop(w);

    // read_log: entry IDs are consecutive from 1 and there is exactly one
    // entry per frame in the file: the begin records are not entries.
    let b = bytes(&path);
    let n_frames: usize = common::spans(&b)
        .iter()
        .map(|s| common::frame_spans(&b, *s).len())
        .sum();
    let log = read_log(&path).unwrap();
    assert_eq!(log.len(), n_frames);
    for (i, e) in log.iter().enumerate() {
        assert_eq!(e.id.0, i as u64 + 1, "entry {i}");
    }
}

// ---------------------------------------------------------------------------
// Edge cases around the begin record (confirm pass)
// ---------------------------------------------------------------------------

#[test]
fn begin_with_valid_crc_but_zero_tag_and_zero_tx_is_corrupt_not_torn() {
    // begin_crc is valid (so it is not "all zeros"); tag 0 != 0x07 -> rule 4.
    let (_d, path) = build();
    let mut file = bytes(&path);
    file.extend_from_slice(&common::begin_raw(0, 0, 0, 0));
    assert_refused(&path, &file, "zero tag+tx, valid crc, nothing after");
    let mut file = bytes(&path);
    file.extend_from_slice(&common::begin_raw(0, 0, 1000, 0));
    assert_refused(&path, &file, "zero tag+tx, body_len past EOF");
}

#[test]
fn body_len_zero_with_valid_crcs_is_corrupt() {
    // An empty body has no Commit: rule 7, never torn (all bytes present).
    let (_d, path) = build();
    let next_tx = common::spans(&bytes(&path)).len() as u64;
    let mut file = bytes(&path);
    file.extend_from_slice(&common::txn(next_tx, &[]));
    assert_refused(&path, &file, "empty body at the end");
}

#[test]
fn body_len_zero_followed_by_a_valid_txn_is_corrupt() {
    let (_d, path) = build();
    let next_tx = common::spans(&bytes(&path)).len() as u64;
    let mut file = bytes(&path);
    file.extend_from_slice(&common::txn(next_tx, &[]));
    // a plausible following txn: copy of the last real txn's body with tx+1
    let orig = bytes(&path);
    let s = *common::spans(&orig).last().unwrap();
    file.extend_from_slice(&common::txn(next_tx + 1, &orig[s.body_start..s.end]));
    assert_refused(&path, &file, "empty body then another txn");
}

#[test]
fn a_committed_txn_followed_by_one_to_24_nonzero_bytes_is_torn_and_cut() {
    let (_d, path) = build();
    // a fifth-plus txn through the real writer so `valid` is a clean log
    let mut w = World::open(&path).unwrap();
    w.create_entity("CHARACTER", Some("lisa")).unwrap();
    drop(w);
    let valid = bytes(&path);
    for n in 1..=24usize {
        let tail = vec![0x5Au8; n];
        assert_torn(&path, &valid, &tail, &format!("{n} nonzero tail bytes"));
    }
    // and the same tail bytes that look like a begin prefix
    let b = common::begin_raw(BEGIN_TAG, 6, 10, 0);
    for n in 1..BEGIN_LEN {
        assert_torn(
            &path,
            &valid,
            &b[..n],
            &format!("begin prefix of {n} bytes"),
        );
    }
}
