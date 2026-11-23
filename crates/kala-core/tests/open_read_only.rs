//! Slice 5 follow-up: `World::open_read_only(path)` and edges at a canon on
//! `View` (RED step, written from the spec in CLAUDE.md "One writer at a
//! time, many readers" and the PM decision, not from the implementation).
//!
//! API under test (does not exist yet; compile errors naming only these are
//! the expected red):
//!   * `World::open_read_only(path: &Path) -> Result<World, Error>`
//!   * `Error::ReadOnly`
//!   * `View::edge(&self, id: EdgeId) -> Option<(EntityId, String, EntityId)>`
//!   * `View::edge_id(&self, source: EntityId, attr: &str, target: EntityId)
//!        -> Option<EdgeId>`
//!
//! Contract of `open_read_only`:
//!   * replays the committed transactions exactly as `World::open` does;
//!   * takes no lock;
//!   * never modifies the file system (no cut, no quarantine, no creation);
//!     `recovery()` reports what `open` WOULD cut, with `quarantine: None`;
//!   * every write fails with `Error::ReadOnly` and appends nothing;
//!   * errors as `World::open` (Corrupt, BadMagic, UnsupportedVersion, Io).
mod common;

use common::{Span, spans};
use kala_core::log::EntryBody;
use kala_core::*;
use proptest::prelude::*;
use std::path::{Path, PathBuf};

const VARROW: &str = include_str!("fixtures/varrow.yaml");

fn bytes(p: &Path) -> Vec<u8> {
    std::fs::read(p).unwrap()
}

/// Every file in `dir` with its bytes, sorted by name.
fn dir_snapshot(dir: &Path) -> Vec<(String, Vec<u8>)> {
    let mut v: Vec<(String, Vec<u8>)> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| {
            let e = e.unwrap();
            (
                e.file_name().to_string_lossy().into_owned(),
                std::fs::read(e.path()).unwrap_or_default(),
            )
        })
        .collect();
    v.sort();
    v
}

fn copy_to(src: &Path) -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("w.kala");
    std::fs::copy(src, &p).unwrap();
    (dir, p)
}

fn varrow_file() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("w.kala");
    drop(load_fixture_to(VARROW, &p).expect("varrow loads"));
    (dir, p)
}

/// Varrow with its last transaction cut `into` bytes after its begin record.
fn varrow_torn(into: usize) -> (tempfile::TempDir, PathBuf, Vec<u8>, usize) {
    let (d, p) = varrow_file();
    let full = bytes(&p);
    let last: Span = *spans(&full).last().unwrap();
    let cut = full[..last.start + into].to_vec();
    std::fs::write(&p, &cut).unwrap();
    (d, p, cut, last.start)
}

/// Varrow with a body byte of its last transaction flipped (all bytes there).
fn varrow_damaged() -> (tempfile::TempDir, PathBuf, Vec<u8>, usize) {
    let (d, p) = varrow_file();
    let mut full = bytes(&p);
    let last: Span = *spans(&full).last().unwrap();
    full[last.body_start + 6] ^= 0x01;
    std::fs::write(&p, &full).unwrap();
    (d, p, full, last.start)
}

// ------------------------------------------------------------ snapshots

const TICKS: [i64; 7] = [i64::MIN, -400, -250, 0, 200, 601, i64::MAX];

fn entities_of(w: &World) -> Vec<EntityId> {
    w.entries()
        .filter_map(|e| match e.body {
            EntryBody::DefineEntity { id, .. } => Some(id),
            _ => None,
        })
        .collect()
}

/// Every read the API offers, at several canons, as one string.
fn snapshot(w: &World) -> String {
    let mut out = format!(
        "tx={:?} tick={:?} recovery_cut={}\n",
        w.latest_tx(),
        w.latest_tick(),
        w.recovery().bytes_cut
    );
    let ents = entities_of(w);
    let attrs: Vec<(String, ValueType)> = w
        .attributes()
        .map(|a| (a.name.clone(), a.value_type))
        .collect();
    let last = w.latest_tx().0;
    let mut canons = vec![0, 1, last / 2, last.saturating_sub(1), last, last + 1];
    canons.sort();
    canons.dedup();
    for c in canons {
        let v = w.at_canon(TxId(c));
        out += &format!(
            "c{c} canon={:?} latest_tick={:?}\n",
            v.canon(),
            v.latest_tick()
        );
        for &e in &ents {
            for (a, ty) in &attrs {
                out += &format!("c{c} {e:?} {a} hist={:?}\n", v.history(e, a));
                for k in TICKS {
                    out += &format!("c{c} {e:?} {a} @{k} {:?}\n", v.as_of(e, a, Tick(k)));
                    if *ty == ValueType::Ref {
                        for d in [Direction::Out, Direction::In, Direction::Both] {
                            let mut n = v.neighbors(e, a, d, Tick(k));
                            if let Ok(n) = n.as_mut() {
                                n.sort();
                            }
                            out += &format!("c{c} {e:?} {a} {d:?} @{k} {n:?}\n");
                        }
                    }
                }
            }
        }
        for name in ["Edwin", "edwin", "Varrow", "Greg", "City"] {
            for a in ["name", "label"] {
                for k in [-400, 0, 601] {
                    out += &format!(
                        "c{c} lookup {a} {name} @{k} {:?} ci={:?}\n",
                        v.lookup(a, &Value::Str(name.into()), Tick(k)),
                        v.lookup_ci(a, name, Tick(k))
                    );
                }
            }
        }
    }
    out += "world-level latest as_of/lookup:\n";
    for &e in &ents {
        for (a, _) in &attrs {
            out += &format!("{e:?} {a} {:?}\n", w.as_of(e, a, w.latest_tick()));
        }
    }
    out += &format!(
        "lookup {:?}\n",
        w.lookup("name", &Value::Str("Edwin".into()), Tick(0))
    );
    for e in w.entries() {
        out += &format!("{e:?}\n");
        if let EntryBody::DefineEdge { id, .. } = e.body {
            out += &format!("edge {id:?} {:?}\n", w.edge(id));
        }
    }
    for h in ["edwin", "greg", "varrow", "lisa", "nobody"] {
        out += &format!("{h}={:?}\n", w.entity_by_handle(h));
    }
    out
}

/// `open_read_only(p)` answers every read as `World::open` of a copy of `p`.
fn assert_same_reads(p: &Path) {
    let (_d, copy) = copy_to(p);
    let expect = World::open(&copy).expect("open of a copy");
    let ro = World::open_read_only(p).expect("open_read_only");
    assert_eq!(snapshot(&ro), snapshot(&expect));
    assert_eq!(ro.recovery().bytes_cut, expect.recovery().bytes_cut);
    assert_eq!(ro.recovery().cause, expect.recovery().cause);
}

// ------------------------------------------------------------ same reads

#[test]
fn reads_of_a_clean_varrow_are_the_same_as_open() {
    let (_d, p) = varrow_file();
    let before = bytes(&p);
    assert_same_reads(&p);
    assert_eq!(bytes(&p), before);
}

#[test]
fn a_clean_log_reports_nothing_cut() {
    let (_d, p) = varrow_file();
    let ro = World::open_read_only(&p).unwrap();
    assert_eq!(*ro.recovery(), RecoveryReport::default());
}

#[test]
fn a_genesis_only_world_opens_read_only_and_is_empty() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("w.kala");
    drop(World::create(&p, Schema::new().node_type("CHARACTER")).unwrap());
    let before = bytes(&p);
    let ro = World::open_read_only(&p).unwrap();
    assert_eq!(ro.latest_tx(), TxId(0));
    assert_eq!(ro.latest_tick(), Tick(0));
    assert_eq!(bytes(&p), before);
}

#[test]
fn reads_on_a_torn_tail_are_the_valid_prefix_as_open_gives() {
    for into in [0usize, 1, 24, 25, 40, 100] {
        let (_d, p, _cut, _keep) = varrow_torn(into);
        assert_same_reads(&p);
    }
}

#[test]
fn reads_on_a_damaged_tail_are_the_valid_prefix_as_open_gives() {
    let (_d, p, _all, _keep) = varrow_damaged();
    assert_same_reads(&p);
}

#[test]
fn reads_on_a_zero_filled_tail_are_the_valid_prefix_as_open_gives() {
    let (_d, p) = varrow_file();
    let mut b = bytes(&p);
    b.extend_from_slice(&[0u8; 64]);
    std::fs::write(&p, &b).unwrap();
    assert_same_reads(&p);
    assert_eq!(bytes(&p), b);
}

// ------------------------------------------------------------ no lock

#[test]
fn succeeds_while_a_writer_world_holds_the_file() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("w.kala");
    let held = load_fixture_to(VARROW, &p).expect("fixture world, lock held");
    assert_eq!(
        World::open(&p).unwrap_err(),
        Error::WorldLocked,
        "precondition"
    );
    let ro = World::open_read_only(&p).expect("no lock taken");
    assert_eq!(ro.latest_tx(), held.latest_tx());
    assert_eq!(ro.latest_tick(), held.latest_tick());
}

#[test]
fn succeeds_while_open_and_create_worlds_hold_the_file() {
    let (_d, p) = varrow_file();
    let held = World::open(&p).unwrap();
    let ro = World::open_read_only(&p).expect("while World::open holds it");
    assert_eq!(ro.latest_tx(), held.latest_tx());
    drop(ro);
    drop(held);

    let dir = tempfile::tempdir().unwrap();
    let q = dir.path().join("c.kala");
    let created = World::create(&q, Schema::new().node_type("CHARACTER")).unwrap();
    let ro = World::open_read_only(&q).expect("while World::create holds it");
    assert_eq!(ro.latest_tx(), created.latest_tx());
}

#[test]
fn two_read_only_opens_work_at_once() {
    let (_d, p) = varrow_file();
    let a = World::open_read_only(&p).unwrap();
    let b = World::open_read_only(&p).unwrap();
    assert_eq!(snapshot(&a), snapshot(&b));
}

#[test]
fn a_live_read_only_world_does_not_stop_open() {
    let (_d, p) = varrow_file();
    let ro = World::open_read_only(&p).unwrap();
    let mut w = World::open(&p).expect("read-only open holds no lock");
    let edwin = w.entity_by_handle("edwin").unwrap();
    w.assert(edwin, "alias", Value::Str("The Grey".into()), Some(Tick(5)))
        .unwrap();
    // and a read-only open can follow while that writer is alive
    let ro2 = World::open_read_only(&p).unwrap();
    assert_eq!(ro2.latest_tx(), w.latest_tx());
    assert_ne!(ro.latest_tx(), w.latest_tx(), "ro is a snapshot at open");
}

#[test]
fn a_read_only_open_after_a_writer_dropped_does_not_hold_the_lock_afterwards() {
    let (_d, p) = varrow_file();
    drop(World::open_read_only(&p).unwrap());
    World::open(&p).expect("lock free");
}

// ------------------------------------------------------------ no changes to disk

#[test]
fn a_torn_tail_is_not_cut_and_nothing_is_quarantined() {
    for into in [0usize, 10, 24, 25, 40, 200] {
        let (d, p, cut, keep) = varrow_torn(into);
        let dir_before = dir_snapshot(d.path());
        let ro = World::open_read_only(&p).unwrap();
        let r = ro.recovery();
        assert_eq!(r.bytes_cut, (cut.len() - keep) as u64, "into={into}");
        // a cut exactly at a transaction boundary is a clean shorter log
        let cause = if into == 0 {
            None
        } else {
            Some(CutCause::Torn)
        };
        assert_eq!(r.cause, cause, "into={into}");
        assert_eq!(r.quarantine, None, "into={into}");
        drop(ro);
        assert_eq!(bytes(&p), cut, "file bytes unchanged, into={into}");
        assert_eq!(
            dir_snapshot(d.path()),
            dir_before,
            "no new files, into={into}"
        );
    }
}

#[test]
fn a_damaged_tail_is_not_cut_and_nothing_is_quarantined() {
    let (d, p, all, keep) = varrow_damaged();
    let dir_before = dir_snapshot(d.path());
    let ro = World::open_read_only(&p).unwrap();
    let r = ro.recovery();
    assert_eq!(r.bytes_cut, (all.len() - keep) as u64);
    assert_eq!(r.cause, Some(CutCause::Damaged));
    assert_eq!(r.quarantine, None);
    drop(ro);
    assert_eq!(bytes(&p), all);
    assert_eq!(dir_snapshot(d.path()), dir_before);
    assert!(
        dir_snapshot(d.path())
            .iter()
            .all(|(n, _)| !n.contains(".torn-")),
        "no quarantine file"
    );
}

#[test]
fn the_report_matches_what_open_does_on_a_copy_and_open_still_cuts_afterwards() {
    let (d, p, cut, keep) = varrow_torn(40);
    let (_d2, copy) = copy_to(&p);
    let on_copy = World::open(&copy).unwrap().recovery().clone();
    let ro = World::open_read_only(&p).unwrap();
    assert_eq!(ro.recovery().bytes_cut, on_copy.bytes_cut);
    assert_eq!(ro.recovery().cause, on_copy.cause);
    drop(ro);
    // a later writer open still does its recovery, with its quarantine
    let w = World::open(&p).unwrap();
    assert_eq!(w.recovery().bytes_cut, (cut.len() - keep) as u64);
    assert!(w.recovery().quarantine.is_some());
    drop(w);
    assert_eq!(bytes(&p), cut[..keep]);
    assert_eq!(dir_snapshot(d.path()).len(), 2);
}

#[test]
fn a_missing_file_is_io_not_found_and_is_not_created() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("absent.kala");
    let e = World::open_read_only(&p).expect_err("must fail");
    assert!(
        matches!(
            e,
            Error::Io {
                kind: std::io::ErrorKind::NotFound,
                ..
            }
        ),
        "{e:?}"
    );
    assert!(!p.exists());
    assert!(dir_snapshot(dir.path()).is_empty());
}

// ------------------------------------------------------------ errors as open

#[test]
fn mid_log_damage_is_corrupt_and_untouched() {
    let (d, p) = varrow_file();
    let mut b = bytes(&p);
    let mid = spans(&b).iter().find(|s| s.tx == 3).copied().unwrap();
    b[mid.body_start + 6] ^= 0x01;
    std::fs::write(&p, &b).unwrap();
    let dir_before = dir_snapshot(d.path());
    assert!(
        matches!(World::open(&copy_to(&p).1), Err(Error::Corrupt(_))),
        "precondition"
    );
    let e = World::open_read_only(&p).expect_err("must fail");
    assert!(matches!(e, Error::Corrupt(_)), "{e:?}");
    assert_eq!(dir_snapshot(d.path()), dir_before);
}

#[test]
fn a_foreign_header_is_bad_magic() {
    let (_d, p) = varrow_file();
    let mut b = bytes(&p);
    b[0] ^= 0xFF;
    std::fs::write(&p, &b).unwrap();
    assert_eq!(World::open_read_only(&p).err(), Some(Error::BadMagic));
    assert_eq!(bytes(&p), b);
}

#[test]
fn an_unknown_version_is_unsupported_version() {
    let (_d, p) = varrow_file();
    let mut b = bytes(&p);
    b[8..12].copy_from_slice(&6u32.to_le_bytes());
    std::fs::write(&p, &b).unwrap();
    assert_eq!(
        World::open_read_only(&p).err(),
        Some(Error::UnsupportedVersion(6))
    );
    assert_eq!(bytes(&p), b);
}

#[test]
fn a_header_only_file_is_corrupt() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("w.kala");
    std::fs::write(&p, common::header(kala_core::log::LOG_FORMAT_VERSION)).unwrap();
    let e = World::open_read_only(&p).expect_err("must fail");
    assert!(matches!(e, Error::Corrupt(_)), "{e:?}");
}

// ------------------------------------------------------------ writes are refused

fn assert_read_only<T: std::fmt::Debug>(r: Result<T, Error>, what: &str) {
    assert_eq!(r.unwrap_err(), Error::ReadOnly, "{what}");
}

fn first_fact_entry(w: &World) -> EntryId {
    w.entries()
        .find(|e| matches!(e.body, EntryBody::Fact { .. }))
        .unwrap()
        .id
}

#[test]
fn every_write_fails_with_read_only_and_appends_nothing() {
    let (d, p) = varrow_file();
    let before = bytes(&p);
    let dir_before = dir_snapshot(d.path());
    let mut ro = World::open_read_only(&p).unwrap();
    let edwin = ro.entity_by_handle("edwin").unwrap();
    let mara = ro.entity_by_handle("mara").unwrap();
    let tx = ro.latest_tx();
    let snap = snapshot(&ro);

    assert_read_only(
        ro.assert(edwin, "alias", Value::Str("X".into()), Some(Tick(1))),
        "assert",
    );
    assert_read_only(
        ro.assert(edwin, "alias", Value::Str("X".into()), None),
        "tick-less assert",
    );
    assert_read_only(
        ro.retract(edwin, "ally_of", Value::Ref(mara), Some(Tick(1))),
        "retract",
    );
    assert_read_only(
        ro.correct(first_fact_entry(&ro), Correction::Delete),
        "correct",
    );
    assert_read_only(
        ro.create_entity("CHARACTER", Some("newbie")),
        "create_entity",
    );
    assert_read_only(
        ro.create_entity("CHARACTER", None),
        "create_entity, no handle",
    );

    // an explicit transaction: some op or the commit reports ReadOnly, and
    // nothing is ever committed
    let mut txn = ro.begin();
    let a = txn.assert(edwin, "alias", Value::Str("Y".into()), Some(Tick(1)));
    let b = txn.create_entity("CHARACTER", Some("another"));
    let committed = txn.commit();
    assert!(
        matches!(a, Err(Error::ReadOnly))
            || matches!(b, Err(Error::ReadOnly))
            || matches!(committed, Err(Error::ReadOnly)),
        "a={a:?} b={b:?} commit={committed:?}"
    );
    assert!(committed.is_err(), "begin+commit must not succeed");

    assert_eq!(ro.latest_tx(), tx);
    assert_eq!(ro.entity_by_handle("newbie"), None);
    assert_eq!(ro.entity_by_handle("another"), None);
    assert_eq!(snapshot(&ro), snap, "in-memory world unchanged");
    drop(ro);
    assert_eq!(bytes(&p), before, "nothing appended");
    assert_eq!(dir_snapshot(d.path()), dir_before);
}

#[test]
fn a_no_op_write_is_still_refused_not_reported_as_zero_changed() {
    let (_d, p) = varrow_file();
    let mut ro = World::open_read_only(&p).unwrap();
    let edwin = ro.entity_by_handle("edwin").unwrap();
    // retract of a value that was never asserted would be "0 changed" on a
    // normal world; here the read-only refusal comes first
    assert_read_only(
        ro.retract(edwin, "alias", Value::Str("never".into()), Some(Tick(1))),
        "retract no-op",
    );
}

#[test]
fn acknowledge_recovery_is_harmless_and_writes_stay_refused() {
    let (d, p, all, _keep) = varrow_damaged();
    let mut ro = World::open_read_only(&p).unwrap();
    let report = ro.recovery().clone();
    ro.acknowledge_recovery();
    assert_eq!(*ro.recovery(), report);
    let edwin = ro.entity_by_handle("edwin").unwrap();
    assert_read_only(
        ro.assert(edwin, "alias", Value::Str("X".into()), Some(Tick(1))),
        "assert after acknowledge",
    );
    drop(ro);
    assert_eq!(bytes(&p), all);
    assert_eq!(dir_snapshot(d.path()).len(), 1);
}

#[test]
fn read_only_error_has_a_message() {
    assert!(!Error::ReadOnly.to_string().trim().is_empty());
}

// ------------------------------------------------------------ snapshot at open

#[test]
fn a_log_appended_after_the_open_is_not_seen_but_a_new_open_sees_it() {
    let (_d, p) = varrow_file();
    let mut writer = World::open(&p).unwrap();
    let ro = World::open_read_only(&p).unwrap();
    let tx0 = ro.latest_tx();
    let snap0 = snapshot(&ro);

    let edwin = writer.entity_by_handle("edwin").unwrap();
    writer
        .assert(
            edwin,
            "alias",
            Value::Str("The Late".into()),
            Some(Tick(9000)),
        )
        .unwrap();
    let lisa = writer
        .create_entity("CHARACTER", Some("late_arrival"))
        .unwrap();
    assert!(writer.latest_tx() > tx0);

    assert_eq!(ro.latest_tx(), tx0);
    assert_eq!(ro.entity_by_handle("late_arrival"), None);
    assert_ne!(ro.latest_tick(), Tick(9000));
    assert_eq!(snapshot(&ro), snap0);

    let ro2 = World::open_read_only(&p).unwrap();
    assert_eq!(ro2.latest_tx(), writer.latest_tx());
    assert_eq!(ro2.latest_tick(), Tick(9000));
    assert_eq!(ro2.entity_by_handle("late_arrival"), Some(lisa));
    assert!(
        ro2.as_of_value(edwin, "alias", &Value::Str("The Late".into()), Tick(9000))
            .unwrap()
    );
}

// ------------------------------------------------------------ View::edge / edge_id

#[test]
fn view_edge_is_visible_from_its_defining_tx_on() {
    let w = load_fixture(VARROW).unwrap();
    let defs: Vec<(TxId, EdgeId, EntityId, AttrId, EntityId)> = w
        .entries()
        .filter_map(|e| match e.body {
            EntryBody::DefineEdge {
                id,
                source,
                attr,
                target,
            } => Some((e.tx, id, source, attr, target)),
            _ => None,
        })
        .collect();
    assert!(defs.len() > 10, "varrow has many edges");
    let mut tx_seen = std::collections::BTreeSet::new();
    for (tx, id, source, attr, target) in defs {
        tx_seen.insert(tx);
        let name = w.attr_name(attr).unwrap().to_string();
        let want = Some((source, name.clone(), target));
        let before = w.at_canon(TxId(tx.0 - 1));
        let at = w.at_canon(tx);
        let latest = w.at_canon(w.latest_tx());
        assert_eq!(
            before.edge(id),
            None,
            "{id:?} before its defining tx#{}",
            tx.0
        );
        assert_eq!(before.edge_id(source, &name, target), None, "{id:?} before");
        assert_eq!(at.edge(id), want, "{id:?} at its defining tx");
        assert_eq!(at.edge_id(source, &name, target), Some(id));
        assert_eq!(latest.edge(id), w.edge(id));
        assert_eq!(
            latest.edge_id(source, &name, target),
            w.edge_id(source, &name, target)
        );
        assert_eq!(latest.edge_id(source, &name, target), Some(id));
        // canon 0 (genesis only) knows no edge
        assert_eq!(w.at_canon(TxId(0)).edge(id), None);
    }
    assert!(
        tx_seen.len() > 2,
        "edges are defined in several transactions"
    );
}

#[test]
fn view_edge_of_an_id_never_defined_is_none() {
    let w = load_fixture(VARROW).unwrap();
    let max = w
        .entries()
        .filter_map(|e| match e.body {
            EntryBody::DefineEdge { id, .. } => Some(id),
            _ => None,
        })
        .last()
        .unwrap();
    let v = w.at_canon(w.latest_tx());
    // the next one after the last defined, as World::edge says
    let beyond = EdgeId::new(max.seq() + 1).unwrap();
    assert_eq!(w.edge(beyond), None);
    assert_eq!(v.edge(beyond), None);
    // a canon past the latest reads as the latest
    assert_eq!(w.at_canon(TxId(w.latest_tx().0 + 5)).edge(max), w.edge(max));
}

#[test]
fn view_edge_id_of_a_bidirectional_pair_is_the_same_either_order() {
    let w = load_fixture(VARROW).unwrap();
    let a = w.entity_by_handle("v01").unwrap();
    let b = w.entity_by_handle("v02").unwrap();
    let v = w.at_canon(w.latest_tx());
    let ab = v.edge_id(a, "ally_of", b);
    assert!(ab.is_some(), "v01 ally_of v02 is an edge");
    assert_eq!(ab, v.edge_id(b, "ally_of", a));
    assert_eq!(ab, w.edge_id(a, "ally_of", b));
    assert_eq!(ab, w.edge_id(b, "ally_of", a));
    let e = v.edge(ab.unwrap()).unwrap();
    assert_eq!(e, w.edge(ab.unwrap()).unwrap());
    assert_eq!(e.1, "ally_of");
}

#[test]
fn view_edge_id_is_none_for_non_edges() {
    let w = load_fixture(VARROW).unwrap();
    let v = w.at_canon(w.latest_tx());
    let edwin = w.entity_by_handle("edwin").unwrap();
    let varrow = w.entity_by_handle("varrow").unwrap();
    assert_eq!(v.edge_id(edwin, "no_such_attr", varrow), None);
    assert_eq!(v.edge_id(edwin, "name", varrow), None, "non-ref attribute");
    assert_eq!(v.edge_id(varrow, "ally_of", varrow), None, "never asserted");
}

#[test]
fn view_edge_agrees_with_world_edge_on_a_read_only_world() {
    let (_d, p) = varrow_file();
    let ro = World::open_read_only(&p).unwrap();
    let v = ro.at_canon(ro.latest_tx());
    for e in ro.entries() {
        if let EntryBody::DefineEdge {
            id,
            source,
            attr,
            target,
        } = e.body
        {
            let name = ro.attr_name(attr).unwrap();
            assert_eq!(v.edge(id), ro.edge(id));
            assert_eq!(v.edge_id(source, name, target), Some(id));
        }
    }
}

// ------------------------------------------------------------ random worlds

fn rand_schema() -> Schema {
    let a = |n, t, k, c| AttrDef::new(n, t, k, c);
    Schema::new()
        .node_type("CHARACTER")
        .node_type("PLACE")
        .attribute(a(
            "located_in",
            ValueType::Ref,
            Kind::State,
            Cardinality::One,
        ))
        .attribute(a("name", ValueType::Str, Kind::State, Cardinality::One))
        .attribute(a("age", ValueType::I64, Kind::State, Cardinality::Many))
}

#[derive(Clone, Debug)]
enum Op {
    Assert(u8, u8, i64),
    Retract(u8, u8, i64),
}

fn op_strategy() -> impl Strategy<Value = Op> {
    let tick = prop_oneof![Just(i64::MIN), Just(i64::MAX), Just(0i64), -300i64..300];
    (any::<bool>(), 0u8..3, 0u8..3, tick).prop_map(|(a, e, v, t)| {
        if a {
            Op::Assert(e, v, t)
        } else {
            Op::Retract(e, v, t)
        }
    })
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(24))]

    /// On random worlds, with or without a torn tail, a read-only open
    /// answers as `World::open` of a copy and leaves the file alone.
    #[test]
    fn random_worlds_read_the_same_as_open(
        ops in proptest::collection::vec(op_strategy(), 0..25),
        cut in proptest::option::of(0usize..80),
    ) {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("w.kala");
        {
            let mut w = World::create(&p, rand_schema()).unwrap();
            let c: Vec<EntityId> = (0..3)
                .map(|i| w.create_entity("CHARACTER", Some(&format!("c{i}"))).unwrap())
                .collect();
            let pl: Vec<EntityId> = (0..3)
                .map(|i| w.create_entity("PLACE", Some(&format!("p{i}"))).unwrap())
                .collect();
            for op in &ops {
                let _ = match *op {
                    Op::Assert(e, v, t) => {
                        if v == 0 {
                            w.assert(c[e as usize], "age", Value::I64(v as i64 + t % 3), Some(Tick(t)))
                        } else {
                            w.assert(c[e as usize], "located_in", Value::Ref(pl[v as usize]), Some(Tick(t)))
                        }
                    }
                    Op::Retract(e, v, t) => {
                        w.retract(c[e as usize], "located_in", Value::Ref(pl[v as usize]), Some(Tick(t)))
                    }
                };
            }
        }
        if let Some(into) = cut {
            let full = bytes(&p);
            let last = *spans(&full).last().unwrap();
            let k = (last.start + into).min(full.len());
            std::fs::write(&p, &full[..k]).unwrap();
        }
        let before = dir_snapshot(dir.path());
        let (_d, copy) = copy_to(&p);
        let expect = World::open(&copy).unwrap();
        let ro = World::open_read_only(&p).unwrap();
        prop_assert_eq!(ro.latest_tx(), expect.latest_tx());
        prop_assert_eq!(ro.latest_tick(), expect.latest_tick());
        prop_assert_eq!(ro.recovery().bytes_cut, expect.recovery().bytes_cut);
        prop_assert_eq!(ro.recovery().cause, expect.recovery().cause);
        prop_assert_eq!(ro.recovery().quarantine.clone(), None);
        prop_assert_eq!(
            ro.entries().collect::<Vec<_>>(),
            expect.entries().collect::<Vec<_>>()
        );
        for tx in 0..=ro.latest_tx().0 + 1 {
            let (a, b) = (ro.at_canon(TxId(tx)), expect.at_canon(TxId(tx)));
            for e in entities_of(&ro) {
                for attr in ["located_in", "name", "age"] {
                    prop_assert_eq!(a.history(e, attr), b.history(e, attr));
                    for t in [i64::MIN, -100, 0, 100, i64::MAX] {
                        prop_assert_eq!(a.as_of(e, attr, Tick(t)), b.as_of(e, attr, Tick(t)));
                    }
                }
                for t in [-100, 0, 100] {
                    prop_assert_eq!(
                        a.neighbors(e, "located_in", Direction::Both, Tick(t)).map(|mut v| { v.sort(); v }),
                        b.neighbors(e, "located_in", Direction::Both, Tick(t)).map(|mut v| { v.sort(); v })
                    );
                }
            }
        }
        drop(ro);
        prop_assert_eq!(dir_snapshot(dir.path()), before);
    }
}
