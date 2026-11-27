//! Slice 4 item 0 confirm: edge cases found while confirming the sort-merge
//! warnings (0a), the recovery gate (0b) and the L4 fix (0c).
//!
//! - `txn_commit_warnings_equal_the_full_before_after_overlap_diff` is the
//!   proof that the touched-only diff equals the naive full diff: random
//!   multi-op transactions (asserts, retracts, deletes, tick moves, value
//!   moves, both at once) over a cardinality-one forward attribute, a
//!   bidirectional attribute (self pairs allowed) and a reverse-one attribute;
//!   the commit's warnings must equal `World::overlaps` after minus before
//!   (and before minus after), exactly, in the documented order.
//! - a Damaged-cut world whose first append after acknowledgement fails;
//! - `doctor_log` on a genesis whose entry IDs go backwards (no panic).
mod common;

use common::{frame_spans, spans};
use kala_core::*;
use proptest::prelude::*;
use proptest::test_runner::{RngAlgorithm, RngSeed};
use std::path::{Path, PathBuf};

fn t(x: i64) -> Option<Tick> {
    Some(Tick(x))
}

fn r(e: EntityId) -> Value {
    Value::Ref(e)
}

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
            AttrDef::new("married_to", ValueType::Ref, Kind::State, Cardinality::One)
                .bidirectional(),
        )
        .attribute(
            AttrDef::new("rules", ValueType::Ref, Kind::State, Cardinality::Many).reverse_one(),
        )
        .attribute(AttrDef::new(
            "name",
            ValueType::Str,
            Kind::State,
            Cardinality::One,
        ))
}

const ATTRS: [&str; 3] = ["located_in", "married_to", "rules"];

fn all_overlaps(w: &World) -> Vec<Overlap> {
    ATTRS.iter().flat_map(|a| w.overlaps(a).unwrap()).collect()
}

type Key = (u64, String, u8, Vec<u8>, Vec<u8>, Tick);

fn key(o: &Overlap) -> Key {
    (
        o.entity.raw(),
        o.attr.clone(),
        o.side as u8,
        o.a.encode().unwrap(),
        o.b.encode().unwrap(),
        o.during.from,
    )
}

fn minus(a: &[Overlap], b: &[Overlap]) -> Vec<Overlap> {
    let mut v: Vec<Overlap> = a.iter().filter(|o| !b.contains(o)).cloned().collect();
    v.sort_by_key(key);
    v
}

// ------------------------------------------------------------ self pairs

fn mem() -> (World, [EntityId; 3]) {
    let mut w = World::in_memory(schema()).unwrap();
    let a = w.create_entity("CHARACTER", Some("edwin")).unwrap();
    let b = w.create_entity("CHARACTER", Some("mara")).unwrap();
    let c = w.create_entity("CHARACTER", Some("lisa")).unwrap();
    (w, [a, b, c])
}

/// Every commit's warnings equal the full diff, step by step.
fn step(w: &mut World, f: impl FnOnce(&mut World) -> Result<WriteResult, Error>) -> WriteResult {
    let before = all_overlaps(w);
    let res = f(w).unwrap();
    let after = all_overlaps(w);
    assert_eq!(res.warnings.introduced, minus(&after, &before));
    assert_eq!(res.warnings.resolved, minus(&before, &after));
    res
}

#[test]
fn bidirectional_self_pair_warnings_match_the_full_diff_through_writes_retracts_and_corrections() {
    let (mut w, [edwin, mara, lisa]) = mem();
    step(&mut w, |w| w.assert(edwin, "married_to", r(edwin), t(0)));
    // a partner on top of the self pair, spelled from the other side
    let m = step(&mut w, |w| w.assert(mara, "married_to", r(edwin), t(5)));
    assert_eq!(m.warnings.introduced.len(), 1, "{:?}", m.warnings);
    let l = step(&mut w, |w| w.assert(edwin, "married_to", r(lisa), t(7)));
    assert!(!l.warnings.introduced.is_empty());
    // the self pair ends: the overlaps that involved it resolve
    let end = step(&mut w, |w| w.retract(edwin, "married_to", r(edwin), t(6)));
    assert!(!end.warnings.resolved.is_empty() || !end.warnings.introduced.is_empty());
    // moving a partner onto the self pair, and back
    let e = m.entries[0];
    step(&mut w, |w| {
        w.correct(
            e,
            Correction::Replace {
                value: Some(r(edwin)),
                tick: None,
                op: None,
                precision: None,
            },
        )
    });
    step(&mut w, |w| {
        w.correct(
            e,
            Correction::Replace {
                value: Some(r(lisa)),
                tick: Some(Tick(-2)),
                op: None,
                precision: None,
            },
        )
    });
    step(&mut w, |w| w.correct(e, Correction::Delete));
}

#[test]
fn a_self_pair_alone_never_warns_and_is_one_edge_in_both_spellings() {
    let (mut w, [edwin, ..]) = mem();
    let a = w.assert(edwin, "married_to", r(edwin), t(0)).unwrap();
    assert_eq!(a.changed, 1);
    assert_eq!(a.warnings, Warnings::default());
    let b = w.assert(edwin, "married_to", r(edwin), t(0)).unwrap();
    assert_eq!(b.changed, 0);
    assert_eq!(b.warnings, Warnings::default());
    assert!(w.overlaps("married_to").unwrap().is_empty());
    assert!(doctor(&w, &DoctorOptions::default()).is_clean());
}

// ------------------------------------------------------------ the property

#[derive(Clone, Debug)]
enum S {
    Assert {
        attr: u8,
        who: u8,
        target: u8,
        tick: i64,
    },
    Retract {
        attr: u8,
        who: u8,
        target: u8,
        tick: i64,
    },
    Delete {
        nth: u8,
    },
    MoveTick {
        nth: u8,
        tick: i64,
    },
    MoveValue {
        nth: u8,
        target: u8,
    },
    MoveBoth {
        nth: u8,
        target: u8,
        tick: i64,
    },
    Flip {
        nth: u8,
    },
}

fn s() -> impl Strategy<Value = S> {
    prop_oneof![
        5 => (0u8..3, 0u8..3, 0u8..3, -3i64..=3).prop_map(|(attr, who, target, tick)| S::Assert { attr, who, target, tick }),
        3 => (0u8..3, 0u8..3, 0u8..3, -3i64..=3).prop_map(|(attr, who, target, tick)| S::Retract { attr, who, target, tick }),
        1 => (0u8..40).prop_map(|nth| S::Delete { nth }),
        2 => (0u8..40, -3i64..=3).prop_map(|(nth, tick)| S::MoveTick { nth, tick }),
        3 => (0u8..40, 0u8..3).prop_map(|(nth, target)| S::MoveValue { nth, target }),
        3 => (0u8..40, 0u8..3, -3i64..=3).prop_map(|(nth, target, tick)| S::MoveBoth { nth, target, tick }),
        1 => (0u8..40).prop_map(|nth| S::Flip { nth }),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 160,
        rng_algorithm: RngAlgorithm::ChaCha,
        rng_seed: RngSeed::Fixed(0x6469_6666),
        failure_persistence: None,
        ..ProptestConfig::default()
    })]

    /// Scripts of transactions (1..6 ops each). A transaction whose write is
    /// refused is dropped and must leave no trace; a committed one reports
    /// exactly `after - before` / `before - after` of the full overlap set.
    #[test]
    fn txn_commit_warnings_equal_the_full_before_after_overlap_diff(
        txns in proptest::collection::vec(proptest::collection::vec(s(), 1..6), 1..14)
    ) {
        let mut w = World::in_memory(schema()).unwrap();
        let people = [
            w.create_entity("CHARACTER", Some("edwin")).unwrap(),
            w.create_entity("CHARACTER", Some("mara")).unwrap(),
            w.create_entity("CHARACTER", Some("lisa")).unwrap(),
        ];
        let places = [
            w.create_entity("PLACE", Some("varrow")).unwrap(),
            w.create_entity("PLACE", Some("ashford")).unwrap(),
            w.create_entity("PLACE", Some("carnath")).unwrap(),
        ];
        let attr_of = |a: u8| ATTRS[a as usize];
        // married_to targets are people (self pair possible); others places
        let target_of = |a: u8, i: u8| if a == 1 { r(people[i as usize]) } else { r(places[i as usize]) };

        // seed one entry per attribute so entries can be mapped to attributes
        let mut attr_ids: Vec<(AttrId, u8)> = Vec::new();
        let mut pool: Vec<(EntryId, u8)> = Vec::new();
        for a in 0..3u8 {
            let res = w.assert(people[0], attr_of(a), target_of(a, 1), t(0)).unwrap();
            let id = res.entries[0];
            if let Some(EntryInfo { body: log::EntryBody::Fact { attr, .. }, .. }) = w.entry(id) {
                attr_ids.push((attr, a));
            }
            pool.push((id, a));
        }
        let idx_of = |w: &World, id: EntryId| -> Option<u8> {
            match w.entry(id)?.body {
                log::EntryBody::Fact { attr, .. } | log::EntryBody::Revised { attr, .. } =>
                    attr_ids.iter().find(|(x, _)| *x == attr).map(|(_, i)| *i),
                _ => None,
            }
        };

        for ops in txns {
            let before = all_overlaps(&w);
            let tx_before = w.latest_tx();
            let committed = {
                let mut tx = w.begin();
                let mut failed = false;
                for op in &ops {
                    let pick = |nth: u8| pool[nth as usize % pool.len()];
                    let res = match *op {
                        S::Assert { attr, who, target, tick } =>
                            tx.assert(people[who as usize], attr_of(attr), target_of(attr, target), t(tick)),
                        S::Retract { attr, who, target, tick } =>
                            tx.retract(people[who as usize], attr_of(attr), target_of(attr, target), t(tick)),
                        S::Delete { nth } => tx.correct(pick(nth).0, Correction::Delete),
                        S::MoveTick { nth, tick } => tx.correct(pick(nth).0,
                            Correction::Replace { value: None, tick: t(tick), op: None, precision: None }),
                        S::MoveValue { nth, target } => {
                            let (id, a) = pick(nth);
                            tx.correct(id, Correction::Replace { value: Some(target_of(a, target)), tick: None, op: None, precision: None })
                        }
                        S::MoveBoth { nth, target, tick } => {
                            let (id, a) = pick(nth);
                            tx.correct(id, Correction::Replace { value: Some(target_of(a, target)), tick: t(tick), op: None, precision: None })
                        }
                        S::Flip { nth } => tx.correct(pick(nth).0,
                            Correction::Replace { value: None, tick: None, op: Some(Op::Retract), precision: None }),
                    };
                    if res.is_err() { failed = true; break; }
                }
                if failed { None } else { tx.commit().ok() }
            };
            let after = all_overlaps(&w);
            match committed {
                Some(c) => {
                    for id in &c.entries {
                        if let Some(a) = idx_of(&w, *id) { pool.push((*id, a)); }
                    }
                    prop_assert_eq!(&c.warnings.introduced, &minus(&after, &before));
                    prop_assert_eq!(&c.warnings.resolved, &minus(&before, &after));
                    if c.changed == 0 {
                        prop_assert!(before == after);
                        prop_assert_eq!(w.latest_tx(), tx_before);
                    }
                }
                None => {
                    prop_assert!(before == after);
                    prop_assert_eq!(w.latest_tx(), tx_before);
                }
            }
            prop_assert!(doctor(&w, &DoctorOptions::default()).errors().is_empty());
        }
    }
}

// ------------------------------------------------------------ Damaged cut + fault

fn bytes(p: &Path) -> Vec<u8> {
    std::fs::read(p).unwrap()
}

fn damaged_world() -> (tempfile::TempDir, PathBuf, World) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("w.kala");
    let mut w = World::create(&path, schema()).unwrap();
    let greg = w.create_entity("CHARACTER", Some("greg")).unwrap();
    w.create_entity("PLACE", Some("varrow")).unwrap();
    w.assert(greg, "name", Value::Str("Greg".into()), t(1))
        .unwrap();
    w.assert(greg, "name", Value::Str("Gregor".into()), t(2))
        .unwrap();
    assert_eq!(w.latest_tx(), TxId(4));
    drop(w);
    let mut b = bytes(&path);
    let last = *spans(&b).last().unwrap();
    b[last.body_start + 6] ^= 0xFF;
    std::fs::write(&path, &b).unwrap();
    let w = World::open(&path).unwrap();
    assert_eq!(w.recovery().cause, Some(CutCause::Damaged));
    assert_eq!(w.latest_tx(), TxId(3));
    (dir, path, w)
}

fn faulted_append_after_acknowledgement(fault: LogFault) {
    let (_d, path, mut w) = damaged_world();
    let greg = w.entity_by_handle("greg").unwrap();
    // The open reported the damaged tail and left it in the file; the
    // acknowledgement licenses the cut, made here before the faulted write.
    common::acknowledge_and_cut(&mut w, &path, &bytes(&path));
    let report = w.recovery().clone();
    let file_after_open = bytes(&path);
    w.inject_log_fault(fault);
    // acknowledged, so the failure is the fault's, not the gate's
    let err = w
        .assert(greg, "name", Value::Str("Z".into()), t(9))
        .unwrap_err();
    assert_ne!(err, Error::RecoveryUnacknowledged, "{fault:?}");
    assert_eq!(w.latest_tx(), TxId(3), "{fault:?}: nothing committed");
    assert_eq!(
        w.as_of(greg, "name", Tick(20)).unwrap(),
        vec![Value::Str("Greg".into())],
        "{fault:?}: reads unchanged"
    );
    assert_eq!(*w.recovery(), report, "{fault:?}: report survives");
    // the failed log refuses further writes, still not the gate
    let again = w.assert(greg, "name", Value::Str("Y".into()), t(10));
    assert!(again.is_err());
    assert_ne!(again.unwrap_err(), Error::RecoveryUnacknowledged);
    drop(w);

    // the file holds exactly the committed prefix and reopens clean and writable
    assert_eq!(bytes(&path), file_after_open, "{fault:?}: file unchanged");
    let mut w2 = World::open(&path).unwrap();
    assert_eq!(*w2.recovery(), RecoveryReport::default(), "{fault:?}");
    assert_eq!(w2.latest_tx(), TxId(3));
    assert!(
        doctor_log(&path, &DoctorOptions::default())
            .unwrap()
            .is_clean()
    );
    let greg = w2.entity_by_handle("greg").unwrap();
    w2.assert(greg, "name", Value::Str("Z".into()), t(9))
        .unwrap();
    assert_eq!(w2.latest_tx(), TxId(4));
}

#[test]
fn damaged_cut_then_acknowledged_write_with_fail_next_write_commits_nothing_and_reopens_clean() {
    faulted_append_after_acknowledgement(LogFault::FailNextWrite);
}

#[test]
fn damaged_cut_then_acknowledged_write_with_fail_next_sync_commits_nothing_and_reopens_clean() {
    faulted_append_after_acknowledgement(LogFault::FailNextSync);
}

// ------------------------------------------------------------ genesis IDs backwards

/// Rewrites txn 0 (the genesis): frame `j` of `n` gets entry ID `f(j, n, old)`;
/// framing and the Commit checksum are recomputed.
fn rewrite_genesis_ids(path: &Path, f: impl Fn(usize, usize, u64) -> u64) {
    let src = bytes(path);
    let sp = spans(&src);
    let mut out = src[..common::HEADER_LEN].to_vec();
    for (i, s) in sp.iter().enumerate() {
        if i != 0 {
            out.extend_from_slice(&src[s.start..s.end]);
            continue;
        }
        let fs = frame_spans(&src, *s);
        let n = fs.len();
        let mut body = Vec::new();
        for (j, (start, end, kind)) in fs.iter().enumerate() {
            let old = u64::from_le_bytes(src[start + 5..start + 13].try_into().unwrap());
            let id = f(j, n, old);
            if j + 1 == n {
                assert_eq!(*kind, 5);
                body.extend(common::commit_frame(
                    common::link(&out),
                    &body.clone(),
                    id,
                    s.tx,
                ));
            } else {
                body.extend(common::frame(*kind, id, &src[start + 13..*end]));
            }
        }
        out.extend(common::txn(s.tx, &body));
    }
    // Later transactions chain from the rewritten genesis (format 6).
    common::rechain(&mut out, 1);
    std::fs::write(path, &out).unwrap();
}

#[test]
fn doctor_log_on_genesis_ids_going_backwards_returns_findings_and_never_panics() {
    type F = fn(usize, usize, u64) -> u64;
    let variants: [(&str, F); 5] = [
        ("all reversed incl. commit", |j, n, _| (n - j) as u64),
        ("entries reversed, commit last", |j, n, _| {
            if j + 1 == n {
                n as u64
            } else {
                (n - 1 - j) as u64
            }
        }),
        ("first two swapped", |j, _, old| match j {
            0 => 2,
            1 => 1,
            _ => old,
        }),
        ("duplicate id", |j, _, old| if j == 1 { 1 } else { old }),
        ("last entry before the previous", |j, n, old| {
            if j + 1 == n { old - 2 } else { old }
        }),
    ];
    for (name, f) in variants {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("w.kala");
        {
            let mut w = World::create(&path, schema()).unwrap();
            let g = w.create_entity("CHARACTER", Some("greg")).unwrap();
            w.assert(g, "name", Value::Str("Greg".into()), t(1))
                .unwrap();
        }
        rewrite_genesis_ids(&path, f);
        let report = doctor_log(&path, &DoctorOptions::default())
            .unwrap_or_else(|e| panic!("{name}: doctor_log returned Err({e:?})"));
        assert!(!report.errors().is_empty(), "{name}: {:?}", report.findings);
        assert!(World::open(&path).is_err(), "{name}: open must refuse");
    }
}
