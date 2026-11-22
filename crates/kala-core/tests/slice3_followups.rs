//! Slice 3, item 0 (slice-2 follow-ups), docs/slice-3-decisions.md:
//!  (a) a damaged LAST transaction is quarantined and cut like a torn tail,
//!      `RecoveryReport.cause` says `Torn` or `Damaged`;
//!  (b) a failed fsync truncates the file back and the writer stays failed;
//!  (d) `CommitResult.edges` / `WriteResult.edges` list newly interned edges.
//! (c) (replay validation) lives in tests/replay_validation.rs.
//!
//! API expected from the implementation (all re-exported at the crate root):
//!   `pub enum CutCause { Torn, Damaged }`            (Clone, Copy, Debug, PartialEq, Eq)
//!   `RecoveryReport { bytes_cut: u64, quarantine: Option<PathBuf>, cause: Option<CutCause> }`
//!   `#[doc(hidden)] pub enum LogFault { FailNextSync, FailNextWrite }`
//!   `#[doc(hidden)] World::inject_log_fault(&mut self, fault: LogFault)`
//!   `CommitResult.edges: Vec<EdgeId>`, `WriteResult.edges: Vec<EdgeId>`
//!
//! Note on `^tx`: nothing here depends on `^tx` index ordering; that arrives
//! with corrections (slice 3 items 2-5).
mod common;

use common::{BEGIN_LEN, BEGIN_TAG, begin_raw, commit_frame, spans, txn};
use kala_core::log::read_log;
use kala_core::*;
use std::path::{Path, PathBuf};

fn schema() -> Schema {
    Schema::new()
        .node_type("CHARACTER")
        .node_type("PLACE")
        .node_type("ITEM")
        .attribute(AttrDef::new(
            "owns",
            ValueType::Ref,
            Kind::State,
            Cardinality::Many,
        ))
        .attribute(
            AttrDef::new("ally_of", ValueType::Ref, Kind::State, Cardinality::Many).bidirectional(),
        )
        .attribute(AttrDef::new(
            "name",
            ValueType::Str,
            Kind::State,
            Cardinality::One,
        ))
}

fn t(x: i64) -> Option<Tick> {
    Some(Tick(x))
}

fn bytes(p: &Path) -> Vec<u8> {
    std::fs::read(p).unwrap()
}

fn qpath(path: &Path, suffix: &str) -> PathBuf {
    let mut s = path.as_os_str().to_owned();
    s.push(suffix);
    PathBuf::from(s)
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

/// genesis + 4 txns (greg, varrow, name @1, name @2); latest_tx == 4.
fn build() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("w.kala");
    let mut w = World::create(&path, schema()).unwrap();
    let greg = w.create_entity("CHARACTER", Some("greg")).unwrap();
    let _v = w.create_entity("PLACE", Some("varrow")).unwrap();
    w.assert(greg, "name", Value::Str("Greg".into()), t(1))
        .unwrap();
    w.assert(greg, "name", Value::Str("Gregor".into()), t(2))
        .unwrap();
    assert_eq!(w.latest_tx(), TxId(4));
    drop(w);
    (dir, path)
}

fn open_ok(p: &Path) -> World {
    match World::open(p) {
        Ok(w) => w,
        Err(e) => panic!("open failed: {e:?}"),
    }
}

fn assert_corrupt_untouched(p: &Path) {
    let before = bytes(p);
    match World::open(p) {
        Err(Error::Corrupt(_)) => {}
        Err(e) => panic!("expected Corrupt, got {e:?}"),
        Ok(_) => panic!("expected Corrupt, open succeeded"),
    }
    assert_eq!(bytes(p), before, "open touched the file");
    assert!(matches!(read_log(p), Err(Error::Corrupt(_))));
    assert_eq!(bytes(p), before, "read_log touched the file");
    assert!(quarantine_files(p).is_empty(), "quarantine file made");
}

/// Flips one body byte of the txn at `s` (begin record untouched), so the
/// body CRC fails while every byte is present and the begin record is valid.
fn damage_body(b: &mut [u8], s: common::Span) {
    assert!(s.end > s.body_start + 6);
    b[s.body_start + 6] ^= 0xFF;
}

// ------------------------------------------------------------------ (a)

#[test]
fn damaged_last_txn_is_quarantined_and_cut_with_cause_damaged() {
    let (_d, path) = build();
    let mut file = bytes(&path);
    let sp = spans(&file);
    let last = *sp.last().unwrap();
    damage_body(&mut file, last);
    std::fs::write(&path, &file).unwrap();
    let cut_bytes = file[last.start..].to_vec();

    let w = open_ok(&path);
    assert_eq!(w.latest_tx(), TxId(3));
    let r = w.recovery();
    assert_eq!(r.cause, Some(CutCause::Damaged));
    assert_eq!(r.bytes_cut, cut_bytes.len() as u64);
    let q = r.quarantine.clone().expect("quarantine path");
    assert_eq!(q, qpath(&path, &format!(".torn-{}", last.start)));
    assert_eq!(std::fs::read(&q).unwrap(), cut_bytes);
    drop(w);
    assert_eq!(bytes(&path), file[..last.start].to_vec());
    // A second open is clean.
    let w = open_ok(&path);
    assert_eq!(w.recovery().cause, None);
    assert_eq!(w.recovery().bytes_cut, 0);
    assert_eq!(quarantine_files(&path).len(), 1);
}

#[test]
fn read_log_ignores_a_damaged_last_txn_without_touching_the_file() {
    let (_d, path) = build();
    let mut file = bytes(&path);
    let last = *spans(&file).last().unwrap();
    let committed = read_log(&path).unwrap();
    damage_body(&mut file, last);
    std::fs::write(&path, &file).unwrap();
    let entries = read_log(&path).expect("damaged tail is ignored, not an error");
    // Everything before the damaged txn, nothing of it.
    let upto = spans(&file).iter().filter(|s| s.tx < last.tx).count();
    assert!(upto == 4);
    assert!(entries.len() < committed.len());
    assert!(committed.starts_with(&entries));
    assert_eq!(bytes(&path), file);
    assert!(quarantine_files(&path).is_empty());
}

#[test]
fn damaged_last_txn_with_trailing_garbage_and_no_begin_record_is_cut_whole() {
    let (_d, path) = build();
    let mut file = bytes(&path);
    let last = *spans(&file).last().unwrap();
    damage_body(&mut file, last);
    file.extend_from_slice(&[0xAB; 7]);
    std::fs::write(&path, &file).unwrap();
    let w = open_ok(&path);
    assert_eq!(w.recovery().cause, Some(CutCause::Damaged));
    assert_eq!(w.recovery().bytes_cut, (file.len() - last.start) as u64);
    let q = w.recovery().quarantine.clone().unwrap();
    assert_eq!(std::fs::read(q).unwrap(), file[last.start..].to_vec());
    assert_eq!(w.latest_tx(), TxId(3));
}

#[test]
fn damaged_last_txn_then_writing_continues_from_the_last_committed() {
    let (_d, path) = build();
    let mut file = bytes(&path);
    let last = *spans(&file).last().unwrap();
    let entries_before = read_log(&path).unwrap().len();
    damage_body(&mut file, last);
    std::fs::write(&path, &file).unwrap();
    let mut w = open_ok(&path);
    let greg = w.entity_by_handle("greg").unwrap();
    w.assert(greg, "name", Value::Str("Z".into()), t(9))
        .unwrap();
    assert_eq!(w.latest_tx(), TxId(4));
    drop(w);
    let log = read_log(&path).unwrap();
    for (i, e) in log.iter().enumerate() {
        assert_eq!(e.id.0, i as u64 + 1);
    }
    assert_eq!(log.len(), entries_before); // same shape: one fact + commit
}

#[test]
fn torn_tail_reports_cause_torn() {
    let (_d, path) = build();
    let file = bytes(&path);
    let last = *spans(&file).last().unwrap();
    let cut_at = last.body_start + 3; // begin intact, body cut short
    std::fs::write(&path, &file[..cut_at]).unwrap();
    let w = open_ok(&path);
    assert_eq!(w.recovery().cause, Some(CutCause::Torn));
    assert_eq!(w.recovery().bytes_cut, (cut_at - last.start) as u64);
}

#[test]
fn torn_tail_shorter_than_a_begin_record_reports_cause_torn() {
    let (_d, path) = build();
    let file = bytes(&path);
    let last = *spans(&file).last().unwrap();
    std::fs::write(&path, &file[..last.start + 10]).unwrap();
    let w = open_ok(&path);
    assert_eq!(w.recovery().cause, Some(CutCause::Torn));
    assert_eq!(w.recovery().bytes_cut, 10);
}

#[test]
fn clean_open_create_and_in_memory_report_no_cause() {
    let (_d, path) = build();
    assert_eq!(open_ok(&path).recovery().cause, None);
    let dir = tempfile::tempdir().unwrap();
    let w = World::create(&dir.path().join("n.kala"), schema()).unwrap();
    assert_eq!(w.recovery().cause, None);
    assert_eq!(World::in_memory(schema()).unwrap().recovery().cause, None);
    assert_eq!(RecoveryReport::default().cause, None);
}

#[test]
fn damage_in_the_middle_followed_by_valid_txns_is_corrupt_and_untouched() {
    let (_d, path) = build();
    let mut file = bytes(&path);
    let sp = spans(&file);
    damage_body(&mut file, sp[2]);
    std::fs::write(&path, &file).unwrap();
    assert_corrupt_untouched(&path);
}

/// A valid begin record (tx greater than the damaged one) right at the next
/// offset after the damaged txn: the damaged txn is not the last, so Corrupt.
#[test]
fn damaged_txn_followed_by_a_valid_begin_record_at_the_next_offset_is_corrupt() {
    let (_d, path) = build();
    let mut file = bytes(&path);
    let last = *spans(&file).last().unwrap();
    damage_body(&mut file, last);
    // A later, well-formed txn (tx 5, commit-only body is enough for framing).
    let entry = read_log(&path).unwrap().last().unwrap().id.0 + 1;
    let body = commit_frame(&[], entry, last.tx + 1);
    file.extend_from_slice(&txn(last.tx + 1, &body));
    std::fs::write(&path, &file).unwrap();
    assert_corrupt_untouched(&path);
}

/// Same, but the valid begin record sits at a non-aligned later offset with
/// garbage between: the scan must look at ANY offset after the damaged body.
#[test]
fn damaged_txn_followed_by_garbage_then_a_valid_begin_record_is_corrupt() {
    for garbage in [1usize, 7, 24, 25, 26, 100] {
        let (_d, path) = build();
        let mut file = bytes(&path);
        let last = *spans(&file).last().unwrap();
        damage_body(&mut file, last);
        file.extend(std::iter::repeat_n(0xEEu8, garbage));
        // Only the begin record itself (tag, tx > damaged tx, correct crc).
        file.extend_from_slice(&begin_raw(BEGIN_TAG, last.tx + 1, 0, 0));
        std::fs::write(&path, &file).unwrap();
        assert_eq!(file.len() - last.end, garbage + BEGIN_LEN);
        assert_corrupt_untouched(&path);
    }
}

/// A begin-record-shaped byte run whose begin_crc is wrong, or whose tx is
/// not greater than the damaged tx, does not count as a later valid begin.
#[test]
fn lookalike_begin_records_after_a_damaged_last_txn_do_not_block_the_cut() {
    for case in ["bad_crc", "tx_not_greater"] {
        let (_d, path) = build();
        let mut file = bytes(&path);
        let last = *spans(&file).last().unwrap();
        damage_body(&mut file, last);
        let mut fake = match case {
            "bad_crc" => begin_raw(BEGIN_TAG, last.tx + 1, 0, 0),
            _ => begin_raw(BEGIN_TAG, last.tx, 0, 0),
        };
        if case == "bad_crc" {
            let n = fake.len();
            fake[n - 1] ^= 0xFF;
        }
        file.extend_from_slice(&fake);
        std::fs::write(&path, &file).unwrap();
        let w = open_ok(&path);
        assert_eq!(w.recovery().cause, Some(CutCause::Damaged), "{case}");
        assert_eq!(w.latest_tx(), TxId(3), "{case}");
    }
}

// ------------------------------------------------------------------ (b)

fn small_world() -> (tempfile::TempDir, PathBuf, World, EntityId) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("w.kala");
    let mut w = World::create(&path, schema()).unwrap();
    let greg = w.create_entity("CHARACTER", Some("greg")).unwrap();
    w.assert(greg, "name", Value::Str("Greg".into()), t(1))
        .unwrap();
    (dir, path, w, greg)
}

#[test]
fn failed_sync_returns_io_refuses_writes_and_leaves_no_trace_after_reopen() {
    let (_d, path, mut w, greg) = small_world();
    let pre_tx = w.latest_tx();
    let pre_len = bytes(&path).len();
    let pre_entries = read_log(&path).unwrap().len();

    w.inject_log_fault(LogFault::FailNextSync);
    let err = w
        .assert(greg, "name", Value::Str("Doomed".into()), t(5))
        .unwrap_err();
    assert!(matches!(err, Error::Io { .. }), "got {err:?}");
    // The writer truncated the file back to its length before the txn.
    assert_eq!(bytes(&path).len(), pre_len);

    // Stays failed: every kind of write is refused.
    assert!(
        w.assert(greg, "name", Value::Str("Other".into()), t(6))
            .is_err()
    );
    assert!(w.create_entity("CHARACTER", None).is_err());
    let mut tx = w.begin();
    let staged = tx.assert(greg, "name", Value::Str("X".into()), t(7));
    let committed = staged.and_then(|_| tx.commit());
    assert!(committed.is_err());
    assert_eq!(bytes(&path).len(), pre_len);
    drop(w);

    let w = open_ok(&path);
    assert_eq!(w.latest_tx(), pre_tx);
    assert_eq!(bytes(&path).len(), pre_len);
    assert_eq!(w.recovery().bytes_cut, 0);
    assert_eq!(w.recovery().cause, None);
    assert!(w.recovery().quarantine.is_none());
    assert!(quarantine_files(&path).is_empty());
    assert_eq!(read_log(&path).unwrap().len(), pre_entries);
    assert!(
        !w.as_of_value(greg, "name", &Value::Str("Doomed".into()), Tick(5))
            .unwrap()
    );
    assert!(
        w.as_of_value(greg, "name", &Value::Str("Greg".into()), Tick(5))
            .unwrap()
    );
}

#[test]
fn failed_sync_of_a_multi_write_txn_with_a_new_entity_leaves_nothing() {
    let (_d, path, mut w, greg) = small_world();
    let pre_tx = w.latest_tx();
    let pre_len = bytes(&path).len();
    w.inject_log_fault(LogFault::FailNextSync);
    let mut tx = w.begin();
    let lisa = tx.create_entity("CHARACTER", Some("lisa")).unwrap();
    tx.assert(lisa, "name", Value::Str("Lisa".into()), t(3))
        .unwrap();
    tx.assert(greg, "name", Value::Str("G2".into()), t(4))
        .unwrap();
    assert!(matches!(tx.commit(), Err(Error::Io { .. })));
    drop(w);
    let w = open_ok(&path);
    assert_eq!(w.latest_tx(), pre_tx);
    assert_eq!(bytes(&path).len(), pre_len);
    assert_eq!(w.entity_by_handle("lisa"), None);
    assert_eq!(w.recovery().bytes_cut, 0);
}

#[test]
fn after_a_failed_sync_reopen_accepts_the_same_write_with_the_same_ids() {
    let (_d, path, mut w, greg) = small_world();
    let entries = read_log(&path).unwrap().len() as u64;
    w.inject_log_fault(LogFault::FailNextSync);
    assert!(
        w.assert(greg, "name", Value::Str("Z".into()), t(5))
            .is_err()
    );
    drop(w);
    let mut w = open_ok(&path);
    let tx = w.latest_tx();
    w.assert(greg, "name", Value::Str("Z".into()), t(5))
        .unwrap();
    assert_eq!(w.latest_tx(), TxId(tx.0 + 1));
    drop(w);
    let log = read_log(&path).unwrap();
    assert_eq!(log.len() as u64, entries + 2);
    assert_eq!(log.last().unwrap().id.0, entries + 2);
}

#[test]
fn failed_write_returns_io_and_the_world_refuses_writes() {
    let (_d, path, mut w, greg) = small_world();
    let pre_tx = w.latest_tx();
    w.inject_log_fault(LogFault::FailNextWrite);
    let err = w
        .assert(greg, "name", Value::Str("Doomed".into()), t(5))
        .unwrap_err();
    assert!(matches!(err, Error::Io { .. }), "got {err:?}");
    assert!(
        w.assert(greg, "name", Value::Str("Other".into()), t(6))
            .is_err()
    );
    assert!(w.create_entity("PLACE", None).is_err());
    drop(w);
    let w = open_ok(&path);
    assert_eq!(w.latest_tx(), pre_tx);
    assert!(
        !w.as_of_value(greg, "name", &Value::Str("Doomed".into()), Tick(5))
            .unwrap()
    );
}

#[test]
fn log_faults_are_ignored_by_in_memory_worlds() {
    let mut w = World::in_memory(schema()).unwrap();
    let greg = w.create_entity("CHARACTER", None).unwrap();
    w.inject_log_fault(LogFault::FailNextSync);
    w.inject_log_fault(LogFault::FailNextWrite);
    let r = w
        .assert(greg, "name", Value::Str("G".into()), t(1))
        .unwrap();
    assert_eq!(r.changed, 1);
    assert!(w.assert(greg, "name", Value::Str("H".into()), t(2)).is_ok());
}

// ------------------------------------------------------------------ (d)

struct W {
    w: World,
    edwin: EntityId,
    mara: EntityId,
    sword: EntityId,
    shield: EntityId,
}

fn world() -> W {
    let mut w = World::in_memory(schema()).unwrap();
    let edwin = w.create_entity("CHARACTER", Some("edwin")).unwrap();
    let mara = w.create_entity("CHARACTER", Some("mara")).unwrap();
    let sword = w.create_entity("ITEM", Some("sword")).unwrap();
    let shield = w.create_entity("ITEM", Some("shield")).unwrap();
    W {
        w,
        edwin,
        mara,
        sword,
        shield,
    }
}

#[test]
fn commit_lists_two_new_edges_in_interning_order() {
    let mut x = world();
    let mut tx = x.w.begin();
    // Written in the order shield, sword: interning order follows it.
    tx.assert(x.edwin, "owns", Value::Ref(x.shield), t(1))
        .unwrap();
    tx.assert(x.edwin, "owns", Value::Ref(x.sword), t(1))
        .unwrap();
    let r = tx.commit().unwrap();
    let e_shield = x.w.edge_id(x.edwin, "owns", x.shield).unwrap();
    let e_sword = x.w.edge_id(x.edwin, "owns", x.sword).unwrap();
    assert_eq!(r.edges, vec![e_shield, e_sword]);
    assert!(e_shield.raw() < e_sword.raw());
    assert_eq!(r.changed, 2);
}

#[test]
fn txn_write_result_has_no_edges_only_the_commit_does() {
    let mut x = world();
    let mut tx = x.w.begin();
    let wr = tx
        .assert(x.edwin, "owns", Value::Ref(x.sword), t(1))
        .unwrap();
    assert!(wr.edges.is_empty());
    let r = tx.commit().unwrap();
    assert_eq!(r.edges.len(), 1);
}

#[test]
fn re_asserting_an_existing_edge_after_retract_lists_no_edge() {
    let mut x = world();
    x.w.assert(x.edwin, "owns", Value::Ref(x.sword), t(1))
        .unwrap();
    x.w.retract(x.edwin, "owns", Value::Ref(x.sword), t(10))
        .unwrap();
    let mut tx = x.w.begin();
    tx.assert(x.edwin, "owns", Value::Ref(x.sword), t(20))
        .unwrap();
    let r = tx.commit().unwrap();
    assert_eq!(r.changed, 1);
    assert!(r.edges.is_empty());
}

#[test]
fn commit_with_one_new_and_one_existing_edge_lists_only_the_new() {
    let mut x = world();
    x.w.assert(x.edwin, "owns", Value::Ref(x.sword), t(1))
        .unwrap();
    x.w.retract(x.edwin, "owns", Value::Ref(x.sword), t(10))
        .unwrap();
    let mut tx = x.w.begin();
    tx.assert(x.edwin, "owns", Value::Ref(x.sword), t(20))
        .unwrap();
    tx.assert(x.edwin, "owns", Value::Ref(x.shield), t(20))
        .unwrap();
    let r = tx.commit().unwrap();
    let e_shield = x.w.edge_id(x.edwin, "owns", x.shield).unwrap();
    assert_eq!(r.edges, vec![e_shield]);
}

#[test]
fn bidirectional_edge_is_listed_once_even_if_written_from_both_sides() {
    let mut x = world();
    let mut tx = x.w.begin();
    tx.assert(x.edwin, "ally_of", Value::Ref(x.mara), t(1))
        .unwrap();
    // Same normalised edge, already held at tick 5: a no-op.
    tx.assert(x.mara, "ally_of", Value::Ref(x.edwin), t(5))
        .unwrap();
    let r = tx.commit().unwrap();
    let id = x.w.edge_id(x.edwin, "ally_of", x.mara).unwrap();
    assert_eq!(x.w.edge_id(x.mara, "ally_of", x.edwin), Some(id));
    assert_eq!(r.edges, vec![id]);
    assert_eq!(r.changed, 1);
}

#[test]
fn bidirectional_edge_written_from_the_larger_side_is_listed_once() {
    let mut x = world();
    let mut tx = x.w.begin();
    tx.assert(x.mara, "ally_of", Value::Ref(x.edwin), t(1))
        .unwrap();
    let r = tx.commit().unwrap();
    let id = x.w.edge_id(x.edwin, "ally_of", x.mara).unwrap();
    assert_eq!(r.edges, vec![id]);
}

#[test]
fn auto_commit_write_result_lists_new_edge() {
    let mut x = world();
    let r =
        x.w.assert(x.edwin, "owns", Value::Ref(x.sword), t(1))
            .unwrap();
    let id = x.w.edge_id(x.edwin, "owns", x.sword).unwrap();
    assert_eq!(r.edges, vec![id]);
    assert_eq!(r.changed, 1);
}

#[test]
fn auto_commit_second_edge_gets_the_next_id_and_existing_edge_lists_none() {
    let mut x = world();
    let r1 =
        x.w.assert(x.edwin, "owns", Value::Ref(x.sword), t(1))
            .unwrap();
    let r2 =
        x.w.assert(x.edwin, "owns", Value::Ref(x.shield), t(1))
            .unwrap();
    assert_eq!(r2.edges.len(), 1);
    assert_eq!(r2.edges[0].seq(), r1.edges[0].seq() + 1);
    x.w.retract(x.edwin, "owns", Value::Ref(x.sword), t(5))
        .unwrap();
    let r3 =
        x.w.assert(x.edwin, "owns", Value::Ref(x.sword), t(9))
            .unwrap();
    assert_eq!(r3.changed, 1);
    assert!(r3.edges.is_empty());
}

#[test]
fn auto_commit_bidirectional_lists_its_single_edge() {
    let mut x = world();
    let r =
        x.w.assert(x.mara, "ally_of", Value::Ref(x.edwin), t(1))
            .unwrap();
    assert_eq!(
        r.edges,
        vec![x.w.edge_id(x.edwin, "ally_of", x.mara).unwrap()]
    );
    let r =
        x.w.assert(x.edwin, "ally_of", Value::Ref(x.mara), t(5))
            .unwrap();
    assert_eq!(r.changed, 0);
    assert!(r.edges.is_empty());
}

#[test]
fn non_ref_writes_list_no_edges() {
    let mut x = world();
    let r =
        x.w.assert(x.edwin, "name", Value::Str("E".into()), t(1))
            .unwrap();
    assert!(r.edges.is_empty());
}

#[test]
fn no_op_commit_lists_no_edges() {
    let mut x = world();
    x.w.assert(x.edwin, "owns", Value::Ref(x.sword), t(1))
        .unwrap();
    // Already held at tick 5 -> no-op, nothing appended.
    let mut tx = x.w.begin();
    tx.assert(x.edwin, "owns", Value::Ref(x.sword), t(5))
        .unwrap();
    let r = tx.commit().unwrap();
    assert_eq!(r.changed, 0);
    assert!(r.edges.is_empty());
    // Empty txn.
    let r = x.w.begin().commit().unwrap();
    assert_eq!(r.changed, 0);
    assert!(r.edges.is_empty());
    // Auto-commit no-op.
    let r =
        x.w.assert(x.edwin, "owns", Value::Ref(x.sword), t(6))
            .unwrap();
    assert_eq!(r.changed, 0);
    assert!(r.edges.is_empty());
}

#[test]
fn retract_of_an_unheld_edge_value_interns_nothing() {
    let mut x = world();
    let r =
        x.w.retract(x.edwin, "owns", Value::Ref(x.sword), t(1))
            .unwrap();
    assert_eq!(r.changed, 0);
    assert!(r.edges.is_empty());
    assert_eq!(x.w.edge_id(x.edwin, "owns", x.sword), None);
}

// ---- item 0 CONFIRM edge cases (sdet) ----

/// Same rule as `lookalike_begin_records_after_a_damaged_last_txn_do_not_block_the_cut`
/// but with a LOWER tx than the damaged txn, and a complete valid-looking
/// begin record: "tx > the damaged tx" is required, so it does not count.
#[test]
fn damaged_last_txn_followed_by_a_valid_begin_with_a_lower_tx_is_cut_as_damaged() {
    for lower in [0u64, 1, 3] {
        let (_d, path) = build();
        let mut file = bytes(&path);
        let last = *spans(&file).last().unwrap();
        assert_eq!(last.tx, 4);
        damage_body(&mut file, last);
        file.extend_from_slice(&begin_raw(BEGIN_TAG, lower, 0, 0));
        std::fs::write(&path, &file).unwrap();
        let w = open_ok(&path);
        assert_eq!(w.recovery().cause, Some(CutCause::Damaged), "tx {lower}");
        assert_eq!(w.latest_tx(), TxId(3), "tx {lower}");
        assert_eq!(w.recovery().bytes_cut, (file.len() - last.start) as u64);
        let q = qpath(&path, &format!(".torn-{}", last.start));
        assert_eq!(w.recovery().quarantine, Some(q.clone()));
        drop(w);
        assert_eq!(bytes(&path), file[..last.start]);
        assert_eq!(
            bytes(&q),
            file[last.start..],
            "quarantine holds txn + the lookalike"
        );
    }
}

/// A valid begin record (tx greater than the damaged tx) lying INSIDE the
/// damaged txn's own body does not count: the scan starts after the body end.
#[test]
fn a_valid_begin_record_inside_the_damaged_body_does_not_block_the_cut() {
    let (_d, path) = build();
    let orig = bytes(&path);
    let last = *spans(&orig).last().unwrap();
    let mut file = orig.clone();
    let embedded = begin_raw(BEGIN_TAG, last.tx + 1, 0, 0);
    let at = last.body_start + 6;
    assert!(
        at + embedded.len() <= last.end,
        "embedded record must lie inside the body"
    );
    file[at..at + embedded.len()].copy_from_slice(&embedded);
    assert_ne!(file, orig);
    assert_eq!(file.len(), orig.len());
    std::fs::write(&path, &file).unwrap();
    let w = open_ok(&path);
    assert_eq!(w.recovery().cause, Some(CutCause::Damaged));
    assert_eq!(w.latest_tx(), TxId(3));
    assert_eq!(w.recovery().bytes_cut, (file.len() - last.start) as u64);
    drop(w);
    assert_eq!(bytes(&path), file[..last.start]);
}

/// Control for the test above: the same record placed just AFTER the damaged
/// body does count -> Corrupt, untouched.
#[test]
fn a_valid_begin_record_just_after_the_damaged_body_is_corrupt_control() {
    let (_d, path) = build();
    let mut file = bytes(&path);
    let last = *spans(&file).last().unwrap();
    damage_body(&mut file, last);
    file.extend_from_slice(&begin_raw(BEGIN_TAG, last.tx + 1, 0, 0));
    std::fs::write(&path, &file).unwrap();
    assert_corrupt_untouched(&path);
}

/// FailNextSync on a txn holding only a create_entity (DefineEntity + Commit,
/// no fact): Io, and after reopen nothing of it is there.
#[test]
fn failed_sync_of_a_create_entity_only_txn_is_io_and_leaves_nothing() {
    let (_d, path, mut w, _greg) = small_world();
    let pre_tx = w.latest_tx();
    let pre_len = bytes(&path).len();
    let pre_entries = read_log(&path).unwrap().len();
    w.inject_log_fault(LogFault::FailNextSync);
    let err = w.create_entity("CHARACTER", Some("lisa")).unwrap_err();
    assert!(matches!(err, Error::Io { .. }), "got {err:?}");
    assert_eq!(bytes(&path).len(), pre_len, "file truncated back");
    assert!(
        w.create_entity("CHARACTER", None).is_err(),
        "writer stays failed"
    );
    drop(w);
    let mut w = open_ok(&path);
    assert_eq!(w.latest_tx(), pre_tx);
    assert_eq!(w.entity_by_handle("lisa"), None);
    assert_eq!(w.recovery().bytes_cut, 0);
    assert_eq!(w.recovery().cause, None);
    assert!(quarantine_files(&path).is_empty());
    assert_eq!(read_log(&path).unwrap().len(), pre_entries);
    // the handle and the sequence number are free again
    let lisa = w.create_entity("CHARACTER", Some("lisa")).unwrap();
    assert_eq!(w.format_id(lisa), "CHARACTER#2");
}
