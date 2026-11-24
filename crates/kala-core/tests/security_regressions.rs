//! Item 17: security regressions (H1 mid-log damage must never silently cut
//! committed transactions; H2 exclusive lock on the log file), re-expressed
//! for log format v2 (docs/slice-2-decisions.md, Item 1).
//!
//! - H1(a) is REMOVED in v2 (the Commit-shaped-tail heuristic): exact framing
//!   replaces it. Damage to bytes that are all present (begin record or body of
//!   ANY txn, the last included) -> `Err(Error::Corrupt(_))`, file untouched;
//!   only a cut-short last txn (<25 bytes, body_len past EOF, zero tail) is cut.
//!   Slice 3 item 0a: a body_crc mismatch under a valid begin record in the
//!   LAST txn (nothing valid after it) is also cut, as `CutCause::Damaged`.
//! - H1(b): before cutting a torn tail, `open` writes the cut bytes to
//!   `<path>.torn-<valid_len>` (`-1`, `-2`, ... if it exists), then cuts.
//! - H2: writers exclude each other through an exclusive lock on the log
//!   file. Since slice 15b the lock is held per commit (and for an open or
//!   a recovery cut), not for a World's lifetime: while it is held, an
//!   `open` -> `Err(Error::WorldLocked)` and nothing is cut. The tests hold
//!   the lock explicitly, as a commit in progress in another process does.
//!   `read_log` takes no lock.

use kala_core::log::read_log;
use kala_core::*;
mod common;

use proptest::prelude::*;
use std::path::{Path, PathBuf};

use common::HEADER_LEN;

fn schema() -> Schema {
    Schema::new().node_type("CHARACTER").attribute(AttrDef::new(
        "name",
        ValueType::Str,
        Kind::State,
        Cardinality::Many,
    ))
}

/// A file-backed world with genesis (tx0) + tx1 (entity) + tx2 + tx3 (facts).
/// Returns the tempdir guard and the log path; the World is dropped (lock freed).
fn build() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("world.kala");
    {
        let mut w = World::create(&path, schema()).expect("create");
        let e = w.create_entity("CHARACTER", Some("edwin")).expect("entity");
        w.assert(e, "name", Value::Str("Edwin".into()), Some(Tick(1)))
            .expect("tx2");
        w.assert(e, "name", Value::Str("Greg".into()), Some(Tick(2)))
            .expect("tx3");
        assert_eq!(w.latest_tx(), TxId(3));
    }
    (dir, path)
}

/// Spans grouped per committed transaction (index 0 = genesis). Element 0 of
/// each group is the 25-byte begin record `(start, start + 25)`; the rest are
/// the body's frames. So `group[0].0` is the txn's start offset and
/// `group[1].0` the offset of its first frame.
fn txns(bytes: &[u8]) -> Vec<Vec<(usize, usize)>> {
    common::spans(bytes)
        .into_iter()
        .map(|s| {
            let mut g = vec![(s.start, s.body_start)];
            g.extend(
                common::frame_spans(bytes, s)
                    .into_iter()
                    .map(|(a, b, _)| (a, b)),
            );
            g
        })
        .collect()
}

fn open_err(path: &Path) -> Error {
    match World::open(path) {
        Err(e) => e,
        Ok(_) => panic!("open unexpectedly succeeded"),
    }
}

/// Holds the log file's writer lock the way a commit in progress in another
/// process does. Dropping the file releases it.
fn hold_lock(path: &Path) -> std::fs::File {
    let f = std::fs::File::open(path).expect("open the log");
    f.try_lock().expect("the lock is free");
    f
}

/// An open that doesn't wait for the lock.
fn open_now_err(path: &Path) -> Error {
    match World::open_waiting(path, std::time::Duration::ZERO) {
        Err(e) => e,
        Ok(_) => panic!("open unexpectedly succeeded"),
    }
}

fn is_corrupt(e: &Error) -> bool {
    matches!(e, Error::Corrupt(_))
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

/// Damages the file with `f`, then asserts open and read_log both refuse
/// with Corrupt and the file is byte-identical afterwards.
fn assert_damage_refused(path: &Path, f: impl FnOnce(&mut Vec<u8>)) {
    let mut bytes = std::fs::read(path).unwrap();
    f(&mut bytes);
    std::fs::write(path, &bytes).unwrap();
    let e = open_err(path);
    assert!(is_corrupt(&e), "open: expected Corrupt, got {e:?}");
    assert_eq!(
        std::fs::read(path).unwrap(),
        bytes,
        "open modified the file"
    );
    let e = read_log(path).expect_err("read_log must refuse");
    assert!(is_corrupt(&e), "read_log: expected Corrupt, got {e:?}");
    assert_eq!(
        std::fs::read(path).unwrap(),
        bytes,
        "read_log modified the file"
    );
    assert!(
        quarantine_files(path).is_empty(),
        "no quarantine on refusal"
    );
}

/// Slice 3 item 0a: the file damaged with `f` has a valid begin record whose
/// body_crc mismatches, in the LAST txn with nothing valid after it. open()
/// cuts it like a torn tail (quarantine first) and reports `Damaged`;
/// read_log ignores it without touching the file.
fn assert_damage_cut(path: &Path, f: impl FnOnce(&mut Vec<u8>), keep: usize, label: &str) {
    let mut bytes = std::fs::read(path).unwrap();
    let committed_expected = {
        let clean = path.with_extension("clean");
        std::fs::write(&clean, &bytes[..keep]).unwrap();
        let v = read_log(&clean).unwrap();
        std::fs::remove_file(&clean).unwrap();
        v
    };
    f(&mut bytes);
    std::fs::write(path, &bytes).unwrap();
    assert_eq!(
        read_log(path).unwrap(),
        committed_expected,
        "{label}: read_log"
    );
    assert_eq!(
        std::fs::read(path).unwrap(),
        bytes,
        "{label}: read_log modified"
    );
    assert!(
        quarantine_files(path).is_empty(),
        "{label}: read_log quarantined"
    );
    let w = match World::open(path) {
        Ok(w) => w,
        Err(e) => panic!("{label}: expected a cut, got {e:?}"),
    };
    assert_eq!(w.recovery().cause, Some(CutCause::Damaged), "{label}");
    assert_eq!(
        w.recovery().bytes_cut,
        (bytes.len() - keep) as u64,
        "{label}"
    );
    let q = qpath(path, &format!(".torn-{keep}"));
    assert_eq!(w.recovery().quarantine, Some(q.clone()), "{label}");
    assert_eq!(w.latest_tx(), TxId(2), "{label}: tx3 is gone");
    drop(w);
    assert_eq!(
        std::fs::read(path).unwrap(),
        &bytes[..keep],
        "{label}: cut file"
    );
    assert_eq!(
        std::fs::read(&q).unwrap(),
        &bytes[keep..],
        "{label}: quarantine"
    );
    let w = World::open(path).unwrap_or_else(|e| panic!("{label}: second open {e:?}"));
    assert_eq!(
        *w.recovery(),
        RecoveryReport::default(),
        "{label}: second open clean"
    );
    drop(w);
    std::fs::remove_file(&q).unwrap();
}

fn len_at(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap())
}

fn set_len(bytes: &mut [u8], at: usize, v: u32) {
    bytes[at..at + 4].copy_from_slice(&v.to_le_bytes());
}

// ---------- T1: length-field damage in a non-last transaction ----------

/// `target` 0 = the begin record (-> begin_crc mismatch), 1 = the length field
/// of the txn's first frame (-> body_crc mismatch). Both are `Corrupt`.
fn length_damage(tx_index: usize, mode: &str) {
    length_damage_at(tx_index, mode, 1);
}

fn length_damage_at(tx_index: usize, mode: &str, target: usize) {
    let (_d, path) = build();
    let start = txns(&std::fs::read(&path).unwrap())[tx_index][target].0;
    assert_damage_refused(&path, |b| match mode {
        "plus256" => b[start + 1] ^= 1,
        "max" => set_len(b, start, u32::MAX),
        "minus1" => {
            let l = len_at(b, start);
            set_len(b, start, l - 1);
        }
        _ => unreachable!(),
    });
}

#[test]
fn t1_tx1_length_plus_256_is_corrupt() {
    length_damage(1, "plus256");
}

#[test]
fn t1_tx1_length_u32_max_is_corrupt() {
    length_damage(1, "max");
}

#[test]
fn t1_tx1_length_minus_1_is_corrupt() {
    length_damage(1, "minus1");
}

#[test]
fn t1_tx2_length_plus_256_is_corrupt() {
    length_damage(2, "plus256");
}

#[test]
fn t1_tx2_length_u32_max_is_corrupt() {
    length_damage(2, "max");
}

#[test]
fn t1_tx2_length_minus_1_is_corrupt() {
    length_damage(2, "minus1");
}

#[test]
fn t1_begin_record_damage_in_a_non_last_txn_is_corrupt() {
    for tx in [1usize, 2] {
        for mode in ["plus256", "max", "minus1"] {
            length_damage_at(tx, mode, 0);
        }
    }
}

// Slice 3 item 0a: split from `t1_length_damage_in_the_last_txn_is_corrupt_too`.
// A damaged frame length inside the last body is a body_crc mismatch under a
// valid begin record: cut as Damaged.
#[test]
fn t1_length_damage_in_the_last_txn_body_is_cut_as_damaged() {
    for mode in ["plus256", "minus1", "max"] {
        let (_d, path) = build();
        let orig = std::fs::read(&path).unwrap();
        let t = txns(&orig);
        let last = t.last().unwrap();
        let (start, keep) = (last[1].0, last[0].0);
        assert_damage_cut(
            &path,
            |b| match mode {
                "plus256" => b[start + 1] ^= 1,
                "max" => set_len(b, start, u32::MAX),
                _ => {
                    let l = len_at(b, start);
                    set_len(b, start, l - 1);
                }
            },
            keep,
            mode,
        );
    }
}

// A damaged BEGIN record of the last txn is a begin_crc mismatch, which slice
// 3 item 0a does not touch (only body_crc mismatches under a valid begin are
// "damaged"): still Corrupt, as slice 2 required.
#[test]
fn t1_begin_record_damage_in_the_last_txn_is_still_corrupt() {
    for mode in ["plus256", "minus1"] {
        length_damage_at(3, mode, 0);
    }
}

// ---------- T2: random bit flip in any non-last committed txn ----------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn t2_bit_flip_before_last_txn_never_drops_committed(
        pos in 0.0f64..1.0,
        bit in 0u8..8,
    ) {
        let (_d, path) = build();
        let orig = std::fs::read(&path).unwrap();
        let t = txns(&orig);
        let last_start = t.last().unwrap()[0].0;
        let off = HEADER_LEN + (pos * (last_start - HEADER_LEN) as f64) as usize;
        prop_assume!(off < last_start);
        let mut bytes = orig.clone();
        bytes[off] ^= 1 << bit;
        std::fs::write(&path, &bytes).unwrap();
        // v2: every single-bit flip in a non-last txn is caught by begin_crc or
        // body_crc, so open must refuse with Corrupt and leave the file alone.
        match World::open(&path) {
            Ok(w) => {
                prop_assert!(false, "open succeeded with damage at offset {} (latest_tx {:?})", off, w.latest_tx());
            }
            Err(e) => {
                prop_assert!(is_corrupt(&e), "expected Corrupt, got {:?} (offset {})", e, off);
            }
        }
        prop_assert_eq!(std::fs::read(&path).unwrap(), bytes, "file modified (offset {})", off);
        prop_assert!(quarantine_files(&path).is_empty());
    }
}

// ---------- T3: torn tail is cut, and the cut bytes are quarantined ----------

// Slice 3 item 0a (was `t3_damaged_last_txn_payload_is_corrupt_not_cut`, slice-1
// finding M1): damage inside the last txn's body with a valid begin record and
// nothing valid after it is cut as Damaged. Damage in the begin record itself
// (begin_crc mismatch) stays Corrupt, kept as its own test below.
#[test]
fn t3_damaged_last_txn_payload_is_cut_as_damaged() {
    let (_d, path) = build();
    let orig = std::fs::read(&path).unwrap();
    let last = txns(&orig).pop().unwrap();
    let keep = last[0].0;
    for at in [last[1].0 + 4 + 2, last[1].0] {
        assert_damage_cut(&path, |b| b[at] ^= 0xFF, keep, &format!("offset {at}"));
        std::fs::write(&path, &orig).unwrap();
    }
}

#[test]
fn t3_damaged_last_txn_begin_record_is_still_corrupt_not_cut() {
    let (_d, path) = build();
    let orig = std::fs::read(&path).unwrap();
    let last = txns(&orig).pop().unwrap();
    assert_damage_refused(&path, |b| b[last[0].0 + 3] ^= 0xFF);
}

// Slice 3 item 0a (was `t3_damaged_last_commit_checksum_is_corrupt_not_cut`).
#[test]
fn t3_damaged_last_commit_checksum_is_cut_as_damaged() {
    let (_d, path) = build();
    let orig = std::fs::read(&path).unwrap();
    let keep = txns(&orig).pop().unwrap()[0].0;
    assert_damage_cut(
        &path,
        |b| *b.last_mut().unwrap() ^= 0x01, // checksum byte of the final Commit
        keep,
        "commit checksum",
    );
}

#[test]
fn t3_plain_truncation_quarantines_exactly_the_cut_bytes() {
    let (_d, path) = build();
    let orig = std::fs::read(&path).unwrap();
    let last = txns(&orig).pop().unwrap();
    let valid_len = last[0].0;
    let torn = &orig[..orig.len() - 7]; // mid-Commit
    std::fs::write(&path, torn).unwrap();
    let w = match World::open(&path) {
        Ok(w) => w,
        Err(e) => panic!("{e:?}"),
    };
    assert_eq!(w.latest_tx(), TxId(2));
    drop(w);
    assert_eq!(std::fs::read(&path).unwrap(), &orig[..valid_len]);
    let q = quarantine_files(&path);
    assert_eq!(q, vec![qpath(&path, &format!(".torn-{valid_len}"))]);
    assert_eq!(std::fs::read(&q[0]).unwrap(), &torn[valid_len..]);
}

#[test]
fn t3_clean_log_creates_no_quarantine_file() {
    let (_d, path) = build();
    let before = std::fs::read(&path).unwrap();
    let w = match World::open(&path) {
        Ok(w) => w,
        Err(e) => panic!("{e:?}"),
    };
    drop(w);
    assert_eq!(std::fs::read(&path).unwrap(), before);
    assert!(quarantine_files(&path).is_empty());
}

#[test]
fn t3_two_recoveries_at_same_valid_len_keep_both_quarantines() {
    let (_d, path) = build();
    let orig = std::fs::read(&path).unwrap();
    let valid_len = orig.len();

    let first = vec![0xABu8; 9];
    let mut b = orig.clone();
    b.extend_from_slice(&first);
    std::fs::write(&path, &b).unwrap();
    drop(World::open(&path).map_err(|e| panic!("{e:?}")).ok());
    assert_eq!(std::fs::read(&path).unwrap(), orig);

    let second = vec![0xCDu8; 5];
    let mut b = orig.clone();
    b.extend_from_slice(&second);
    std::fs::write(&path, &b).unwrap();
    drop(World::open(&path).map_err(|e| panic!("{e:?}")).ok());
    assert_eq!(std::fs::read(&path).unwrap(), orig);

    assert_eq!(quarantine_files(&path).len(), 2);
    let base = qpath(&path, &format!(".torn-{valid_len}"));
    let suffixed = qpath(&path, &format!(".torn-{valid_len}-1"));
    assert_eq!(
        std::fs::read(&base).unwrap(),
        first,
        "first quarantine overwritten"
    );
    assert_eq!(std::fs::read(&suffixed).unwrap(), second);
}

// ---------- T4: zero padding after the last Commit ----------

#[test]
fn t4_zero_tail_is_cut_and_quarantined() {
    let (_d, path) = build();
    let orig = std::fs::read(&path).unwrap();
    let mut bytes = orig.clone();
    bytes.extend_from_slice(&[0u8; 4096]);
    std::fs::write(&path, &bytes).unwrap();
    let w = match World::open(&path) {
        Ok(w) => w,
        Err(e) => panic!("zeros must not look like committed data: {e:?}"),
    };
    assert_eq!(w.latest_tx(), TxId(3));
    drop(w);
    assert_eq!(std::fs::read(&path).unwrap(), orig);
    let q = quarantine_files(&path);
    assert_eq!(q, vec![qpath(&path, &format!(".torn-{}", orig.len()))]);
    assert_eq!(std::fs::read(&q[0]).unwrap(), vec![0u8; 4096]);
}

#[test]
fn t4_read_log_ignores_zero_tail() {
    let (_d, path) = build();
    let mut bytes = std::fs::read(&path).unwrap();
    let n = read_log(&path).unwrap().len();
    bytes.extend_from_slice(&[0u8; 4096]);
    std::fs::write(&path, &bytes).unwrap();
    assert_eq!(read_log(&path).unwrap().len(), n);
    assert_eq!(std::fs::read(&path).unwrap(), bytes);
}

// ---------- H2: exclusive lock ----------

#[test]
fn h2_second_open_while_created_world_alive_is_locked() {
    // Slice 15b: a created world holds the lock only while it works, so
    // what excludes the second open is the lock itself, held here as a
    // commit in progress would hold it.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("world.kala");
    let first = World::create(&path, schema()).unwrap();
    let before = std::fs::read(&path).unwrap();
    let held = hold_lock(&path);
    assert_eq!(open_now_err(&path), Error::WorldLocked);
    assert_eq!(std::fs::read(&path).unwrap(), before);
    drop(held);
    assert!(
        World::open(&path).is_ok(),
        "open while the idle creator lives"
    );
    drop(first);
    assert!(World::open(&path).is_ok(), "open after drop must succeed");
}

#[test]
fn h2_second_open_while_opened_world_alive_is_locked() {
    let (_d, path) = build();
    let first = World::open(&path)
        .map_err(|e| panic!("{e:?}"))
        .ok()
        .unwrap();
    let before = std::fs::read(&path).unwrap();
    let held = hold_lock(&path);
    assert_eq!(open_now_err(&path), Error::WorldLocked);
    assert_eq!(std::fs::read(&path).unwrap(), before);
    drop(held);
    drop(first);
    let again = World::open(&path)
        .map_err(|e| panic!("{e:?}"))
        .ok()
        .unwrap();
    assert_eq!(again.latest_tx(), TxId(3));
}

#[test]
fn h2_locked_open_does_not_cut_a_torn_tail() {
    let (_d, path) = build();
    let first = World::open(&path)
        .map_err(|e| panic!("{e:?}"))
        .ok()
        .unwrap();
    // A second handle must not truncate or quarantine behind the back of
    // whoever holds the lock.
    let mut bytes = std::fs::read(&path).unwrap();
    bytes.extend_from_slice(&[0xEE; 6]);
    std::fs::write(&path, &bytes).unwrap();
    let held = hold_lock(&path);
    assert_eq!(open_now_err(&path), Error::WorldLocked);
    assert_eq!(std::fs::read(&path).unwrap(), bytes);
    assert!(quarantine_files(&path).is_empty());
    drop(held);
    drop(first);
}

#[test]
fn h2_read_log_works_while_world_holds_the_lock() {
    let (_d, path) = build();
    let w = World::open(&path)
        .map_err(|e| panic!("{e:?}"))
        .ok()
        .unwrap();
    assert!(!read_log(&path).unwrap().is_empty());
    drop(w);
    let dir = tempfile::tempdir().unwrap();
    let p2 = dir.path().join("w2.kala");
    let w2 = World::create(&p2, schema()).unwrap();
    assert!(!read_log(&p2).unwrap().is_empty());
    drop(w2);
}

// ---------- Edge cases (confirm step) ----------

/// Last committed entry ID of the log at `path`.
fn last_entry_id(path: &Path) -> u64 {
    read_log(path).unwrap().last().unwrap().id.0
}

/// A well-formed Commit frame (length 21, kind 5) with the given entry ID,
/// tx and a deliberately wrong checksum.
fn commit_frame(entry: u64, tx: u64) -> Vec<u8> {
    let mut f = Vec::new();
    f.extend_from_slice(&21u32.to_le_bytes());
    f.push(5);
    f.extend_from_slice(&entry.to_le_bytes());
    f.extend_from_slice(&tx.to_le_bytes());
    f.extend_from_slice(&0x4141_4141u32.to_le_bytes());
    f
}

#[cfg(unix)]
#[test]
fn quarantine_failure_fails_open_with_io_and_leaves_log_untouched() {
    use std::os::unix::fs::PermissionsExt;
    let (dir, path) = build();
    let mut bytes = std::fs::read(&path).unwrap();
    bytes.extend_from_slice(&[0xEE; 9]);
    std::fs::write(&path, &bytes).unwrap();

    struct Restore<'a>(&'a Path);
    impl Drop for Restore<'_> {
        fn drop(&mut self) {
            let _ = std::fs::set_permissions(self.0, std::fs::Permissions::from_mode(0o755));
        }
    }
    let _restore = Restore(dir.path());
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o555)).unwrap();
    // Root ignores permissions: skip if the directory is still writable.
    let probe = dir.path().join("probe");
    if std::fs::File::create(&probe).is_ok() {
        let _ = std::fs::remove_file(&probe);
        eprintln!("skipped: running with permissions-ignoring privileges");
        return;
    }

    let e = open_err(&path);
    assert!(matches!(e, Error::Io { .. }), "expected Io, got {e:?}");
    assert_eq!(
        std::fs::read(&path).unwrap(),
        bytes,
        "log modified despite failed quarantine"
    );
    assert!(quarantine_files(&path).is_empty());
}

// Rewritten from slice 1 (`torn_tail_with_commit_shaped_bytes_for_next_tx_is_refused_not_cut`):
// the H1(a) heuristic is gone. A tail of >= 25 bytes that is not a valid begin
// record (begin_crc mismatch, not all zero) is refused as damage, whatever it
// contains; nothing is cut.
#[test]
fn tail_of_25_or_more_bytes_with_a_bad_begin_crc_is_corrupt_not_cut() {
    for (junk_before, entry_delta, tx) in [(3usize, 1u64, 5u64), (10, 2, 5), (0, 1, 5)] {
        let (_d, path) = build();
        let l = last_entry_id(&path);
        let mut tail = vec![0xEE; junk_before];
        tail.extend(commit_frame(l + entry_delta, tx));
        tail.extend([0xEE; 5]);
        let mut bytes = std::fs::read(&path).unwrap();
        bytes.extend_from_slice(&tail);
        std::fs::write(&path, &bytes).unwrap();
        let e = open_err(&path);
        assert!(is_corrupt(&e), "expected Corrupt, got {e:?}");
        assert_eq!(std::fs::read(&path).unwrap(), bytes, "file modified");
        assert!(quarantine_files(&path).is_empty());
        let e = read_log(&path).expect_err("read_log must refuse too");
        assert!(is_corrupt(&e), "{e:?}");
    }
}

// New in v2: the H1(a) false refusal is gone. A Commit-shaped frame that is
// merely SHORT (< 25 bytes) is just a torn tail: cut and quarantined.
#[test]
fn commit_shaped_tail_shorter_than_a_begin_record_is_cut_not_refused() {
    let (_d, path) = build();
    let l = last_entry_id(&path);
    let orig = std::fs::read(&path).unwrap();
    let mut tail = commit_frame(l + 1, 4);
    tail.truncate(common::BEGIN_LEN - 1);
    let mut bytes = orig.clone();
    bytes.extend_from_slice(&tail);
    std::fs::write(&path, &bytes).unwrap();
    let w = match World::open(&path) {
        Ok(w) => w,
        Err(e) => panic!("short tail must be cut, got {e:?}"),
    };
    assert_eq!(w.latest_tx(), TxId(3));
    assert_eq!(w.recovery().bytes_cut, tail.len() as u64);
    drop(w);
    assert_eq!(std::fs::read(&path).unwrap(), orig);
    let q = quarantine_files(&path);
    assert_eq!(q, vec![qpath(&path, &format!(".torn-{}", orig.len()))]);
    assert_eq!(std::fs::read(&q[0]).unwrap(), tail);
}

// Replaces slice 1's `own_bad_commit_at_{low,high}_entry_edge_is_cut` and
// `own_bad_commit_with_padding_at_both_entry_edges_is_cut` (they probed the
// entry-ID window of the removed heuristic). v2 equivalent of "own torn txn is
// cut": a VALID begin record (tx 4) whose body was cut short at every length.
#[test]
fn own_txn_with_valid_begin_and_cut_short_body_is_cut_and_quarantined() {
    let (_d, path) = build();
    let l = last_entry_id(&path);
    let orig = std::fs::read(&path).unwrap();
    let body = common::commit_frame(common::link(&orig), &[], l + 1, 4);
    let full = common::txn(4, &body);
    for keep in 0..full.len() {
        // keep == 0..25: begin itself cut; 25..: begin + partial body
        let tail = &full[..keep];
        let mut bytes = orig.clone();
        bytes.extend_from_slice(tail);
        std::fs::write(&path, &bytes).unwrap();
        let w = match World::open(&path) {
            Ok(w) => w,
            Err(e) => panic!("keep={keep}: torn txn must be cut, got {e:?}"),
        };
        assert_eq!(w.latest_tx(), TxId(3), "keep={keep}");
        assert_eq!(w.recovery().bytes_cut, keep as u64, "keep={keep}");
        drop(w);
        assert_eq!(std::fs::read(&path).unwrap(), orig, "keep={keep}");
        for q in quarantine_files(&path) {
            let _ = std::fs::remove_file(q);
        }
    }
}

#[test]
fn damage_inside_genesis_with_later_commits_is_corrupt() {
    let (_d, path) = build();
    let g = txns(&std::fs::read(&path).unwrap())[0].clone();
    // A payload byte of genesis's first frame.
    assert_damage_refused(&path, |b| b[g[1].0 + 4 + 2] ^= 0xFF);
    assert_damage_refused(&path, |b| b[g[0].0 + 3] ^= 0xFF);
}

#[test]
fn damage_to_genesis_commit_checksum_with_later_commits_is_corrupt() {
    let (_d, path) = build();
    let g = txns(&std::fs::read(&path).unwrap())[0].clone();
    let end = g.last().unwrap().1;
    assert_damage_refused(&path, |b| b[end - 1] ^= 0x01);
}

#[test]
fn tail_shorter_than_a_begin_record_is_cut_and_quarantined() {
    for n in [1usize, 4, 12, 24] {
        let (_d, path) = build();
        let orig = std::fs::read(&path).unwrap();
        let mut bytes = orig.clone();
        let tail: Vec<u8> = (0..n).map(|i| 0xA0u8.wrapping_add(i as u8)).collect();
        assert!(n < common::BEGIN_LEN);
        bytes.extend_from_slice(&tail);
        std::fs::write(&path, &bytes).unwrap();
        let w = match World::open(&path) {
            Ok(w) => w,
            Err(e) => panic!("tail of {n} bytes must be cut: {e:?}"),
        };
        assert_eq!(w.latest_tx(), TxId(3), "tail {n}");
        drop(w);
        assert_eq!(std::fs::read(&path).unwrap(), orig, "tail {n}");
        let q = quarantine_files(&path);
        assert_eq!(q, vec![qpath(&path, &format!(".torn-{}", orig.len()))]);
        assert_eq!(std::fs::read(&q[0]).unwrap(), tail, "tail {n}");
    }
}

#[test]
fn second_open_after_a_cut_is_clean_and_makes_no_new_quarantine() {
    let (_d, path) = build();
    let orig = std::fs::read(&path).unwrap();
    let mut bytes = orig.clone();
    bytes.extend_from_slice(&[0x77; 24]); // < 25 bytes: torn, not damage
    std::fs::write(&path, &bytes).unwrap();
    drop(World::open(&path).map_err(|e| panic!("{e:?}")).ok());
    let q1 = quarantine_files(&path);
    assert_eq!(q1.len(), 1);
    let q1_bytes = std::fs::read(&q1[0]).unwrap();

    let w = World::open(&path)
        .map_err(|e| panic!("{e:?}"))
        .ok()
        .unwrap();
    assert_eq!(w.latest_tx(), TxId(3));
    drop(w);
    assert_eq!(std::fs::read(&path).unwrap(), orig);
    assert_eq!(quarantine_files(&path), q1, "second open added a file");
    assert_eq!(std::fs::read(&q1[0]).unwrap(), q1_bytes);
}
