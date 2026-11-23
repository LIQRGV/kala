//! Slice 3, item 1 (docs/slice-3-decisions.md): entry IDs in write results and
//! `World::entry(id)`. RED until the implementation lands.
//!
//! API expected (all re-exported at the crate root; `EntryId` and `EntryBody`
//! are used from `kala_core::log`):
//!   `WriteResult  { changed: usize, tick: Tick, entries: Vec<EntryId>, edges: Vec<EdgeId>, .. }`
//!   `CommitResult { tx: TxId, changed: usize, entries: Vec<EntryId>, edges: Vec<EdgeId>, .. }`
//!   `World::entry(&self, id: EntryId) -> Option<EntryInfo>`
//!   `pub struct EntryInfo { pub id: EntryId, pub tx: TxId, pub body: EntryBody }`  (Clone, Debug, PartialEq)
//!   `EntryInfo::revision(&self) -> Option<(EntryId, u32)>`
//!
//! Not tested here: `warnings` (item 7) and corrections (item 2): Deleted /
//! Revised entries and `revision() == Some(..)` arrive with them.

mod common;

use kala_core::log::{EntryBody, EntryId, read_log};
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

fn file_world() -> (tempfile::TempDir, PathBuf, World) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("w.kala");
    let w = World::create(&path, schema()).unwrap();
    (dir, path, w)
}

/// The committed entries of a file log with their tx: `(id, tx, body)`.
fn log_with_tx(path: &Path) -> Vec<(EntryId, TxId, EntryBody)> {
    let log = read_log(path).unwrap();
    let mut out = Vec::new();
    let mut pending = Vec::new();
    for e in log {
        let commit_tx = match &e.body {
            EntryBody::Commit { tx, .. } => Some(*tx),
            _ => None,
        };
        pending.push(e);
        if let Some(tx) = commit_tx {
            for p in pending.drain(..) {
                out.push((p.id, tx, p.body));
            }
        }
    }
    assert!(pending.is_empty(), "read_log returns whole txns only");
    out
}

fn last_id(path: &Path) -> u64 {
    read_log(path).unwrap().last().unwrap().id.0
}

fn is_fact(b: &EntryBody) -> bool {
    matches!(b, EntryBody::Fact { .. })
}

/// What one scripted write reported.
#[derive(Debug, PartialEq)]
struct Reported {
    label: &'static str,
    changed: usize,
    entries: Vec<EntryId>,
    edges: Vec<EdgeId>,
}

fn rep_w(label: &'static str, r: WriteResult) -> Reported {
    Reported {
        label,
        changed: r.changed,
        entries: r.entries,
        edges: r.edges,
    }
}

fn rep_c(label: &'static str, r: CommitResult) -> Reported {
    Reported {
        label,
        changed: r.changed,
        entries: r.entries,
        edges: r.edges,
    }
}

/// The same writes for every world kind.
fn script(w: &mut World) -> Vec<Reported> {
    let mut out = Vec::new();
    let greg = w.create_entity("CHARACTER", Some("greg")).unwrap();
    let varrow = w.create_entity("PLACE", Some("varrow")).unwrap();
    out.push(rep_w(
        "assert new edge",
        w.assert(greg, "located_in", Value::Ref(varrow), t(-300))
            .unwrap(),
    ));
    out.push(rep_w(
        "repeat assert (no-op)",
        w.assert(greg, "located_in", Value::Ref(varrow), t(-299))
            .unwrap(),
    ));
    out.push(rep_w(
        "retract",
        w.retract(greg, "located_in", Value::Ref(varrow), t(-250))
            .unwrap(),
    ));
    out.push(rep_w(
        "assert name",
        w.assert(greg, "name", Value::Str("Greg".into()), t(0))
            .unwrap(),
    ));
    let mut tx = w.begin();
    let lisa = tx.create_entity("CHARACTER", Some("lisa")).unwrap();
    tx.assert(lisa, "name", Value::Str("Lisa".into()), t(1))
        .unwrap();
    tx.assert(greg, "ally_of", Value::Ref(lisa), t(5)).unwrap();
    out.push(rep_c(
        "txn: entity + 2 facts + new edge",
        tx.commit().unwrap(),
    ));
    out.push(rep_w(
        "retract of an unheld value (no-op)",
        w.retract(greg, "name", Value::Str("Nobody".into()), t(9))
            .unwrap(),
    ));
    out
}

// ---------------------------------------------------------------- results

#[test]
fn auto_commit_assert_on_an_existing_edge_target_lists_exactly_its_fact_entry() {
    let (_d, path, mut w) = file_world();
    let greg = w.create_entity("CHARACTER", Some("greg")).unwrap();
    let last = last_id(&path);
    let r = w
        .assert(greg, "name", Value::Str("Greg".into()), t(1))
        .unwrap();
    // Fact = last + 1, Commit = last + 2; only the Fact is listed.
    assert_eq!(r.entries, vec![EntryId(last + 1)]);
    assert_eq!(last_id(&path), last + 2);
    assert!(r.edges.is_empty());
}

#[test]
fn a_new_edge_define_is_not_listed_but_shifts_the_fact_entry_id() {
    let (_d, path, mut w) = file_world();
    let greg = w.create_entity("CHARACTER", Some("greg")).unwrap();
    let varrow = w.create_entity("PLACE", Some("varrow")).unwrap();
    let last = last_id(&path);
    let r = w
        .assert(greg, "located_in", Value::Ref(varrow), t(-300))
        .unwrap();
    // DefineEdge = last + 1, Fact = last + 2, Commit = last + 3.
    assert_eq!(r.entries, vec![EntryId(last + 2)]);
    assert_eq!(r.edges.len(), 1);
    assert_eq!(last_id(&path), last + 3);
}

#[test]
fn no_op_write_lists_no_entries_and_appends_nothing() {
    let (_d, path, mut w) = file_world();
    let greg = w.create_entity("CHARACTER", Some("greg")).unwrap();
    w.assert(greg, "name", Value::Str("Greg".into()), t(1))
        .unwrap();
    let last = last_id(&path);
    let tx = w.latest_tx();
    let r = w
        .assert(greg, "name", Value::Str("Greg".into()), t(2))
        .unwrap();
    assert_eq!(r.changed, 0);
    assert!(r.entries.is_empty());
    let r = w
        .retract(greg, "name", Value::Str("Nobody".into()), t(2))
        .unwrap();
    assert_eq!(r.changed, 0);
    assert!(r.entries.is_empty());
    assert_eq!(last_id(&path), last);
    assert_eq!(w.latest_tx(), tx);
    assert_eq!(w.entry(EntryId(last + 1)), None);
}

#[test]
fn txn_write_results_have_empty_entries_and_the_commit_result_has_them() {
    let (_d, path, mut w) = file_world();
    let greg = w.create_entity("CHARACTER", Some("greg")).unwrap();
    let varrow = w.create_entity("PLACE", Some("varrow")).unwrap();
    let last = last_id(&path);
    let mut tx = w.begin();
    let a = tx
        .assert(greg, "located_in", Value::Ref(varrow), t(-300))
        .unwrap();
    let b = tx
        .assert(greg, "name", Value::Str("Greg".into()), t(0))
        .unwrap();
    for r in [&a, &b] {
        assert_eq!(r.changed, 1);
        assert!(r.entries.is_empty(), "entry IDs are assigned at commit");
        assert!(r.edges.is_empty());
    }
    let c = tx.commit().unwrap();
    // DefineEdge = last+1, Fact(located_in) = last+2, Fact(name) = last+3
    assert_eq!(c.entries, vec![EntryId(last + 2), EntryId(last + 3)]);
    assert_eq!(c.changed, 2);
    assert_eq!(c.edges.len(), 1);
    assert_eq!(last_id(&path), last + 4);
}

#[test]
fn txn_with_new_entity_and_edge_lists_only_fact_entries_in_log_order() {
    let (_d, path, mut w) = file_world();
    let greg = w.create_entity("CHARACTER", Some("greg")).unwrap();
    let last = last_id(&path);
    let mut tx = w.begin();
    let lisa = tx.create_entity("CHARACTER", Some("lisa")).unwrap();
    tx.assert(lisa, "name", Value::Str("Lisa".into()), t(1))
        .unwrap();
    tx.assert(greg, "ally_of", Value::Ref(lisa), t(5)).unwrap();
    let c = tx.commit().unwrap();
    // Format 5 follows operation order, defining each ID before its first use:
    // DefineEntity(lisa) = last+1, Fact name = last+2, DefineEdge = last+3,
    // Fact ally_of = last+4, Commit = last+5.
    assert_eq!(c.entries, vec![EntryId(last + 2), EntryId(last + 4)]);
    let log = log_with_tx(&path);
    let facts_of_tx: Vec<EntryId> = log
        .iter()
        .filter(|(_, tx, b)| *tx == c.tx && is_fact(b))
        .map(|(id, _, _)| *id)
        .collect();
    assert_eq!(c.entries, facts_of_tx);
}

#[test]
fn txn_with_a_no_op_and_a_real_write_lists_only_the_real_one() {
    let (_d, path, mut w) = file_world();
    let greg = w.create_entity("CHARACTER", Some("greg")).unwrap();
    w.assert(greg, "name", Value::Str("Greg".into()), t(1))
        .unwrap();
    let last = last_id(&path);
    let mut tx = w.begin();
    assert_eq!(
        tx.assert(greg, "name", Value::Str("Greg".into()), t(2))
            .unwrap()
            .changed,
        0
    );
    tx.assert(greg, "name", Value::Str("Gregor".into()), t(3))
        .unwrap();
    let c = tx.commit().unwrap();
    assert_eq!(c.changed, 1);
    assert_eq!(c.entries, vec![EntryId(last + 1)]);
}

#[test]
fn all_no_op_commit_and_empty_commit_list_no_entries() {
    let (_d, path, mut w) = file_world();
    let greg = w.create_entity("CHARACTER", Some("greg")).unwrap();
    w.assert(greg, "name", Value::Str("Greg".into()), t(1))
        .unwrap();
    let last = last_id(&path);
    let tx_before = w.latest_tx();
    let mut tx = w.begin();
    tx.assert(greg, "name", Value::Str("Greg".into()), t(2))
        .unwrap();
    let c = tx.commit().unwrap();
    assert_eq!((c.changed, c.tx), (0, tx_before));
    assert!(c.entries.is_empty() && c.edges.is_empty());
    let c = w.begin().commit().unwrap();
    assert!(c.entries.is_empty() && c.edges.is_empty());
    assert_eq!(last_id(&path), last);
}

#[test]
fn a_commit_that_only_creates_an_entity_lists_no_entries() {
    let (_d, path, mut w) = file_world();
    let last = last_id(&path);
    let mut tx = w.begin();
    tx.create_entity("CHARACTER", Some("greg")).unwrap();
    let c = tx.commit().unwrap();
    assert!(c.entries.is_empty(), "a DefineEntity is not a listed entry");
    assert_eq!(last_id(&path), last + 2); // DefineEntity + Commit
}

#[test]
fn reported_entry_ids_equal_the_fact_entries_read_log_shows() {
    let (_d, path, mut w) = file_world();
    let reported = script(&mut w);
    let all: Vec<EntryId> = reported.iter().flat_map(|r| r.entries.clone()).collect();
    let in_log: Vec<EntryId> = log_with_tx(&path)
        .into_iter()
        .filter(|(_, _, b)| is_fact(b))
        .map(|(id, _, _)| id)
        .collect();
    assert_eq!(all, in_log, "{reported:#?}");
    // and every listed id resolves to a Fact with the same body read_log has
    let log = read_log(&path).unwrap();
    for id in &all {
        let info = w.entry(*id).expect("listed entry exists");
        assert!(is_fact(&info.body));
        assert_eq!(info.id, *id);
        assert_eq!(info.body, log[(id.0 - 1) as usize].body);
    }
    // sanity of the script itself: no-ops report nothing
    for r in &reported {
        if r.changed == 0 {
            assert!(r.entries.is_empty(), "{}", r.label);
        } else {
            assert_eq!(r.entries.len(), r.changed, "{}", r.label);
        }
    }
}

#[test]
fn entry_ids_are_ascending_in_every_result() {
    let (_d, _path, mut w) = file_world();
    let reported = script(&mut w);
    let flat: Vec<u64> = reported
        .iter()
        .flat_map(|r| r.entries.iter().map(|e| e.0))
        .collect();
    assert!(flat.windows(2).all(|p| p[0] < p[1]), "{flat:?}");
}

// ----------------------------------------------- in-memory == file-backed

#[test]
fn in_memory_and_file_worlds_assign_identical_entry_ids_for_the_same_writes() {
    let (_d, path, mut fw) = file_world();
    let mut mw = World::in_memory(schema()).unwrap();
    let fr = script(&mut fw);
    let mr = script(&mut mw);
    assert_eq!(fr, mr);
    let n = last_id(&path);
    for id in 1..=n + 1 {
        assert_eq!(fw.entry(EntryId(id)), mw.entry(EntryId(id)), "entry#{id}");
    }
    assert!(mw.entry(EntryId(n)).is_some());
    assert!(mw.entry(EntryId(n + 1)).is_none());
}

#[test]
fn in_memory_world_has_the_genesis_entries_and_commit_tx0_first() {
    let mut mw = World::in_memory(schema()).unwrap();
    let (_d, path, fw) = file_world();
    let genesis = log_with_tx(&path);
    assert!(genesis.len() >= 3);
    for (id, tx, body) in &genesis {
        let info = mw.entry(*id).unwrap_or_else(|| panic!("entry#{}", id.0));
        assert_eq!((info.id, info.tx), (*id, *tx));
        assert_eq!(info.tx, TxId(0));
        assert_eq!(&info.body, body);
    }
    assert!(matches!(
        mw.entry(genesis.last().unwrap().0).unwrap().body,
        EntryBody::Commit { .. }
    ));
    // first write continues right after genesis, as in a file-backed world
    let g = mw.create_entity("CHARACTER", None).unwrap();
    let r = mw.assert(g, "name", Value::Str("G".into()), t(1)).unwrap();
    let mut fw = fw;
    let g = fw.create_entity("CHARACTER", None).unwrap();
    let rf = fw.assert(g, "name", Value::Str("G".into()), t(1)).unwrap();
    assert_eq!(r.entries, rf.entries);
}

// ------------------------------------------------------------ entry(id)

#[test]
fn entry_returns_every_committed_entry_including_defines_and_commits_like_read_log() {
    let (_d, path, mut w) = file_world();
    script(&mut w);
    let log = log_with_tx(&path);
    assert!(
        log.iter()
            .any(|(_, _, b)| matches!(b, EntryBody::DefineNodeType { .. }))
    );
    assert!(
        log.iter()
            .any(|(_, _, b)| matches!(b, EntryBody::DefineAttr { .. }))
    );
    assert!(
        log.iter()
            .any(|(_, _, b)| matches!(b, EntryBody::DefineEntity { .. }))
    );
    assert!(
        log.iter()
            .any(|(_, _, b)| matches!(b, EntryBody::DefineEdge { .. }))
    );
    assert!(
        log.iter()
            .any(|(_, _, b)| matches!(b, EntryBody::Commit { .. }))
    );
    for (id, tx, body) in &log {
        let info = w.entry(*id).unwrap_or_else(|| panic!("entry#{}", id.0));
        assert_eq!(info.id, *id);
        assert_eq!(info.tx, *tx, "entry#{}", id.0);
        assert_eq!(&info.body, body, "entry#{}", id.0);
        assert_eq!(info.revision(), None, "base entries have no revision");
    }
}

#[test]
fn entry_tx_is_the_enclosing_transactions_tx() {
    let (_d, _path, mut w) = file_world();
    let greg = w.create_entity("CHARACTER", Some("greg")).unwrap();
    let r1 = w
        .assert(greg, "name", Value::Str("A".into()), t(1))
        .unwrap();
    let r2 = w
        .assert(greg, "name", Value::Str("B".into()), t(2))
        .unwrap();
    let tx1 = w.entry(r1.entries[0]).unwrap().tx;
    let tx2 = w.entry(r2.entries[0]).unwrap().tx;
    assert_eq!(tx2.0, tx1.0 + 1);
    assert_eq!(tx2, w.latest_tx());
}

#[test]
fn unknown_zero_and_beyond_last_committed_ids_are_none() {
    let (_d, path, mut w) = file_world();
    script(&mut w);
    let n = last_id(&path);
    assert_eq!(w.entry(EntryId(0)), None);
    assert!(w.entry(EntryId(1)).is_some());
    assert!(w.entry(EntryId(n)).is_some());
    assert_eq!(w.entry(EntryId(n + 1)), None);
    assert_eq!(w.entry(EntryId(n + 1000)), None);
    assert_eq!(w.entry(EntryId(u64::MAX)), None);
    let mw = World::in_memory(schema()).unwrap();
    assert_eq!(mw.entry(EntryId(0)), None);
    assert_eq!(mw.entry(EntryId(u64::MAX)), None);
}

#[test]
fn entries_survive_reopen_unchanged_and_ids_continue() {
    let (_d, path, mut w) = file_world();
    script(&mut w);
    let n = last_id(&path);
    let before: Vec<Option<EntryInfo>> = (0..=n + 1).map(|i| w.entry(EntryId(i))).collect();
    drop(w);
    let mut w = World::open(&path).unwrap();
    let after: Vec<Option<EntryInfo>> = (0..=n + 1).map(|i| w.entry(EntryId(i))).collect();
    assert_eq!(before, after);
    // the next write continues after the last entry, and is listed
    let greg = w.entity_by_handle("greg").unwrap();
    let r = w
        .assert(greg, "name", Value::Str("Zed".into()), t(50))
        .unwrap();
    assert_eq!(r.entries, vec![EntryId(n + 1)]);
    assert_eq!(
        w.entry(EntryId(n + 1)).unwrap().body,
        read_log(&path).unwrap()[n as usize].body
    );
}

// ------------------------------------------- dropped / failed transactions

#[test]
fn entries_of_a_dropped_txn_never_appear() {
    let (_d, path, mut w) = file_world();
    let greg = w.create_entity("CHARACTER", Some("greg")).unwrap();
    let n = last_id(&path);
    {
        let mut tx = w.begin();
        let lisa = tx.create_entity("CHARACTER", Some("lisa")).unwrap();
        tx.assert(lisa, "name", Value::Str("Lisa".into()), t(1))
            .unwrap();
        tx.assert(greg, "ally_of", Value::Ref(lisa), t(2)).unwrap();
        // dropped without commit
    }
    for i in 1..=6 {
        assert_eq!(w.entry(EntryId(n + i)), None, "entry#{}", n + i);
    }
    assert_eq!(last_id(&path), n);
    // the next real commit lists exactly what read_log shows
    let r = w
        .assert(greg, "name", Value::Str("Greg".into()), t(3))
        .unwrap();
    let info = w.entry(r.entries[0]).unwrap();
    assert_eq!(
        info.body,
        read_log(&path).unwrap()[(r.entries[0].0 - 1) as usize].body
    );
    assert!(matches!(info.body, EntryBody::Fact { .. }));
    assert_eq!(w.entity_by_handle("lisa"), None);
}

#[test]
fn entries_of_a_poisoned_txn_that_fails_to_commit_never_appear() {
    let (_d, path, mut w) = file_world();
    let greg = w.create_entity("CHARACTER", Some("greg")).unwrap();
    let n = last_id(&path);
    let mut tx = w.begin();
    tx.assert(greg, "name", Value::Str("Greg".into()), t(1))
        .unwrap();
    assert!(
        tx.assert(greg, "no_such_attr", Value::Bool(true), t(1))
            .is_err()
    );
    assert!(tx.commit().is_err());
    assert_eq!(w.entry(EntryId(n + 1)), None);
    assert_eq!(last_id(&path), n);
}

#[test]
fn a_failed_auto_commit_write_appends_no_entry() {
    let (_d, path, mut w) = file_world();
    let greg = w.create_entity("CHARACTER", Some("greg")).unwrap();
    let n = last_id(&path);
    assert!(w.assert(greg, "name", Value::I64(1), t(1)).is_err());
    assert_eq!(w.entry(EntryId(n + 1)), None);
    assert_eq!(last_id(&path), n);
    let r = w
        .assert(greg, "name", Value::Str("Greg".into()), t(1))
        .unwrap();
    assert_eq!(r.entries, vec![EntryId(n + 1)]);
}

// ------------------------------------------- task A: item 1 edge cases

fn snapshot(w: &World, upto: u64) -> Vec<Option<EntryInfo>> {
    (0..=upto).map(|i| w.entry(EntryId(i))).collect()
}

/// The successful writes shared by the fault tests: a create and one fact.
fn base_writes(w: &mut World) -> (EntityId, EntityId) {
    let greg = w.create_entity("CHARACTER", Some("greg")).unwrap();
    let varrow = w.create_entity("PLACE", Some("varrow")).unwrap();
    w.assert(greg, "name", Value::Str("Greg".into()), t(1))
        .unwrap();
    (greg, varrow)
}

fn failed_commit_then_reopen_matches_in_memory(fault: LogFault) {
    let (_d, path, mut w) = file_world();
    let (greg, varrow) = base_writes(&mut w);
    let n = last_id(&path);
    w.inject_log_fault(fault);
    // an edge-defining write: a failed commit must not leak DefineEdge or Fact
    assert!(
        w.assert(greg, "located_in", Value::Ref(varrow), t(-300))
            .is_err()
    );
    assert_eq!(w.entry(EntryId(n + 1)), None, "{fault:?}: Fact leaked");
    assert_eq!(w.entry(EntryId(n + 2)), None, "{fault:?}: entry leaked");
    // a failed multi-entry txn too
    let mut tx = w.begin();
    let _ = tx.assert(greg, "name", Value::Str("Never".into()), t(2));
    assert!(tx.commit().is_err());
    assert_eq!(w.entry(EntryId(n + 1)), None);
    drop(w);

    let mut w = World::open(&path).unwrap();
    assert_eq!(last_id(&path), n, "{fault:?}: log grew");
    assert_eq!(w.entry(EntryId(n + 1)), None);
    let greg = w.entity_by_handle("greg").unwrap();
    let varrow = w.entity_by_handle("varrow").unwrap();
    // the same write now succeeds and takes the next IDs
    let r = w
        .assert(greg, "located_in", Value::Ref(varrow), t(-300))
        .unwrap();
    // DefineEdge = n+1, Fact = n+2
    assert_eq!(r.entries, vec![EntryId(n + 2)], "{fault:?}");
    assert_eq!(r.edges.len(), 1);

    // an in-memory world given only the successful writes is identical
    let mut mw = World::in_memory(schema()).unwrap();
    let (mg, mv) = base_writes(&mut mw);
    let mr = mw
        .assert(mg, "located_in", Value::Ref(mv), t(-300))
        .unwrap();
    assert_eq!(mr.entries, r.entries);
    assert_eq!(mr.edges, r.edges);
    let top = last_id(&path);
    assert_eq!(top, n + 3);
    assert_eq!(snapshot(&w, top + 1), snapshot(&mw, top + 1));
    // and survives another reopen unchanged
    let before = snapshot(&w, top + 1);
    drop(w);
    let w = World::open(&path).unwrap();
    assert_eq!(snapshot(&w, top + 1), before);
}

#[test]
fn after_fail_next_sync_entry_n_plus_1_is_none_and_reopen_continues_ids_like_in_memory() {
    failed_commit_then_reopen_matches_in_memory(LogFault::FailNextSync);
}

#[test]
fn after_fail_next_write_entry_n_plus_1_is_none_and_reopen_continues_ids_like_in_memory() {
    failed_commit_then_reopen_matches_in_memory(LogFault::FailNextWrite);
}

#[test]
fn a_failed_world_still_answers_entry_for_committed_entries() {
    let (_d, path, mut w) = file_world();
    let (greg, _v) = base_writes(&mut w);
    let n = last_id(&path);
    let before = snapshot(&w, n + 1);
    w.inject_log_fault(LogFault::FailNextSync);
    assert!(
        w.assert(greg, "name", Value::Str("Doomed".into()), t(5))
            .is_err()
    );
    assert_eq!(snapshot(&w, n + 1), before);
}

fn two_txn_file() -> (tempfile::TempDir, PathBuf, u64, Vec<Option<EntryInfo>>) {
    let (d, path, mut w) = file_world();
    let (greg, _v) = base_writes(&mut w);
    let n = last_id(&path); // last committed entry before the doomed txn
    w.assert(greg, "name", Value::Str("Gregor".into()), t(2))
        .unwrap();
    let top = last_id(&path);
    assert_eq!(top, n + 2);
    let snap = snapshot(&w, top + 1);
    drop(w);
    (d, path, n, snap)
}

fn assert_cut_txn_invisible(path: &Path, n: u64, snap: &[Option<EntryInfo>]) {
    let w = World::open(path).unwrap();
    assert!(w.recovery().bytes_cut > 0);
    // entries up to n are exactly as before; the cut txn's Fact and Commit are gone
    for i in 0..=n {
        assert_eq!(w.entry(EntryId(i)), snap[i as usize], "entry#{i}");
    }
    assert_eq!(w.entry(EntryId(n + 1)), None);
    assert_eq!(w.entry(EntryId(n + 2)), None);
    assert_eq!(w.entry(EntryId(n + 3)), None);
    assert_eq!(read_log(path).unwrap().last().unwrap().id.0, n);
}

#[test]
fn entry_after_reopening_a_torn_tail_shows_nothing_of_the_cut_txn() {
    let (_d, path, n, snap) = two_txn_file();
    let mut file = std::fs::read(&path).unwrap();
    let last = *common::spans(&file).last().unwrap();
    file.truncate(last.end - 3); // tear inside the Commit frame
    std::fs::write(&path, &file).unwrap();
    assert_cut_txn_invisible(&path, n, &snap);
    // the next write reuses the cut ids
    let mut w = World::open(&path).unwrap();
    let greg = w.entity_by_handle("greg").unwrap();
    let r = w
        .assert(greg, "name", Value::Str("Other".into()), t(3))
        .unwrap();
    assert_eq!(r.entries, vec![EntryId(n + 1)]);
}

#[test]
fn entry_after_reopening_a_torn_begin_record_shows_nothing_of_the_cut_txn() {
    let (_d, path, n, snap) = two_txn_file();
    let mut file = std::fs::read(&path).unwrap();
    let last = *common::spans(&file).last().unwrap();
    file.truncate(last.start + 10);
    std::fs::write(&path, &file).unwrap();
    assert_cut_txn_invisible(&path, n, &snap);
}

#[test]
fn entry_after_reopening_a_damaged_tail_shows_nothing_of_the_cut_txn() {
    let (_d, path, n, snap) = two_txn_file();
    let mut file = std::fs::read(&path).unwrap();
    let last = *common::spans(&file).last().unwrap();
    file[last.body_start + 6] ^= 0xFF;
    std::fs::write(&path, &file).unwrap();
    assert_cut_txn_invisible(&path, n, &snap);
    let w = World::open(&path).unwrap();
    assert_eq!(w.recovery().cause, None, "second open is clean");
}

#[test]
fn entry_bodies_match_between_in_memory_and_file_worlds_for_edge_properties_and_reversed_bidirectional()
 {
    fn run(w: &mut World) {
        let edwin = w.create_entity("CHARACTER", Some("edwin")).unwrap(); // smaller
        let mara = w.create_entity("CHARACTER", Some("mara")).unwrap();
        let carnath = w.create_entity("PLACE", Some("carnath")).unwrap();
        w.assert(edwin, "located_in", Value::Ref(carnath), t(-300))
            .unwrap();
        let edge: EntityId = w.edge_id(edwin, "located_in", carnath).unwrap().into();
        // edge property fact: the entity is the edge id
        w.assert(edge, "name", Value::Str("visit".into()), t(-300))
            .unwrap();
        // bidirectional written reversed (larger entity first)
        w.assert(mara, "ally_of", Value::Ref(edwin), t(-208))
            .unwrap();
        // the same pair from the other side is a no-op
        assert_eq!(
            w.assert(edwin, "ally_of", Value::Ref(mara), t(-208))
                .unwrap()
                .changed,
            0
        );
    }
    let (_d, path, mut fw) = file_world();
    let mut mw = World::in_memory(schema()).unwrap();
    run(&mut fw);
    run(&mut mw);
    let n = last_id(&path);
    for id in 0..=n + 1 {
        assert_eq!(fw.entry(EntryId(id)), mw.entry(EntryId(id)), "entry#{id}");
    }
    // the bidirectional fact is stored normalised (entity < value) in both
    let mut found = false;
    for id in 1..=n {
        if let Some(EntryInfo {
            body:
                EntryBody::Fact {
                    entity,
                    value: Value::Ref(v),
                    op: Op::Assert,
                    tick: Tick(-208),
                    ..
                },
            ..
        }) = mw.entry(EntryId(id))
        {
            assert!(entity < v, "stored reversed: {entity:?} {v:?}");
            found = true;
        }
    }
    assert!(found, "bidirectional fact entry not found");
    // and the same after reopen
    drop(fw);
    let fw = World::open(&path).unwrap();
    for id in 0..=n + 1 {
        assert_eq!(fw.entry(EntryId(id)), mw.entry(EntryId(id)), "entry#{id}");
    }
}
