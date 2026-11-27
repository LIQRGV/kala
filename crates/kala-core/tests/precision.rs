//! Slice 4, item 5: tick precision (docs/slice-4-decisions.md "Item 5"; CLAUDE.md
//! "Vague time"). RED until the implementation lands: this file fails to COMPILE
//! (no `Precision`, `*_with_precision`, `as_of_detail`, `Change::precision`,
//! `Correction::Replace::precision`, `EntryBody::{Fact, Revised}::precision`).
//!
//! API expected:
//!
//! ```text
//! // module tick, re-exported at the crate root
//! pub enum Precision { Exact, Day, Month, Year, Decade, Century }
//!     // Clone, Copy, Debug, PartialEq, Eq, Hash, Default = Exact
//! World::assert_with_precision(&mut self, e: EntityId, attr: &str, v: Value,
//!                              tick: Option<Tick>, p: Precision) -> Result<WriteResult, Error>
//! World::retract_with_precision(..same..)
//! Txn::assert_with_precision / retract_with_precision      (same arguments)
//! Change { tick, op, value, precision: Precision }
//! World::as_of_detail(&self, e: EntityId, attr: &str, t: Tick) -> Result<Vec<Change>, Error>
//! View::as_of_detail(..same..)
//! Correction::Replace { value, tick, op, precision: Option<Precision> }
//! log::EntryBody::Fact { .., precision: Precision }, EntryBody::Revised { .., precision: Precision }
//! log::LOG_FORMAT_VERSION == 4
//! ```
//!
//! Log v4: `Fact` and `Revised` bodies carry `precision u8` right after the op byte
//! (0 exact, 1 day, 2 month, 3 year, 4 decade, 5 century; anything else is Corrupt).
//! Replay rule: a same-key `Revised` must change the op or the precision.
//! Precision is a payload, never part of a key.
mod common;

use common::{commit_frame, frame, txn};
use kala_core::log::{EntryBody, LOG_FORMAT_VERSION, read_log};
use kala_core::*;
use proptest::prelude::*;
use proptest::test_runner::{RngAlgorithm, RngSeed};
use std::collections::HashSet;
use std::path::PathBuf;

const ALL: [Precision; 6] = [
    Precision::Exact,
    Precision::Day,
    Precision::Month,
    Precision::Year,
    Precision::Decade,
    Precision::Century,
];

fn t(x: i64) -> Option<Tick> {
    Some(Tick(x))
}
fn s(x: &str) -> Value {
    Value::Str(x.into())
}

fn schema() -> Schema {
    Schema::new()
        .node_type("CHARACTER")
        .node_type("PLACE")
        .attribute(AttrDef::new(
            "name",
            ValueType::Str,
            Kind::State,
            Cardinality::Many,
        ))
        .attribute(AttrDef::new(
            "located_in",
            ValueType::Ref,
            Kind::State,
            Cardinality::Many,
        ))
        .attribute(AttrDef::new(
            "bought",
            ValueType::Ref,
            Kind::Event,
            Cardinality::Many,
        ))
}

fn clean(w: &World) {
    let rep = doctor(w, &DoctorOptions::default());
    assert!(rep.is_clean(), "{:#?}", rep.findings);
}

fn mem() -> (World, EntityId, EntityId) {
    let mut w = World::in_memory(schema()).unwrap();
    let g = w.create_entity("CHARACTER", Some("greg")).unwrap();
    let v = w.create_entity("PLACE", Some("varrow")).unwrap();
    (w, g, v)
}

fn file() -> (tempfile::TempDir, PathBuf, World, EntityId, EntityId) {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("w.kala");
    let mut w = World::create(&p, schema()).unwrap();
    let g = w.create_entity("CHARACTER", Some("greg")).unwrap();
    let v = w.create_entity("PLACE", Some("varrow")).unwrap();
    (dir, p, w, g, v)
}

fn entry(r: &WriteResult, i: usize) -> EntryId {
    r.entries[i]
}

/// `(tick, op, value, precision)` rows of a history, in the order returned.
fn hist(w: &World, e: EntityId, a: &str) -> Vec<(i64, Op, Value, Precision)> {
    w.history(e, a)
        .unwrap()
        .into_iter()
        .map(|c| (c.tick.0, c.op, c.value, c.precision))
        .collect()
}

fn detail(w: &World, e: EntityId, a: &str, at: i64) -> Vec<(i64, Value, Precision)> {
    w.as_of_detail(e, a, Tick(at))
        .unwrap()
        .into_iter()
        .map(|c| {
            assert_eq!(
                c.op,
                Op::Assert,
                "as_of_detail lists holding values: asserts"
            );
            (c.tick.0, c.value, c.precision)
        })
        .collect()
}

fn rep(
    value: Option<Value>,
    tick: Option<Tick>,
    op: Option<Op>,
    precision: Option<Precision>,
) -> Correction {
    Correction::Replace {
        value,
        tick,
        op,
        precision,
    }
}

fn body_precision(w: &World, id: EntryId) -> Precision {
    match w.entry(id).unwrap().body {
        EntryBody::Fact { precision, .. } | EntryBody::Revised { precision, .. } => precision,
        other => panic!("entry#{} carries no precision: {other:?}", id.0),
    }
}

// ================================================================ the type

#[test]
fn precision_has_six_distinct_copy_hashable_variants_and_defaults_to_exact() {
    assert_eq!(Precision::default(), Precision::Exact);
    let set: HashSet<Precision> = ALL.into_iter().collect();
    assert_eq!(set.len(), 6);
    let p = Precision::Year;
    let q = p; // Copy
    assert_eq!(p, q);
    assert_ne!(Precision::Day, Precision::Month);
    assert!(!format!("{:?}", Precision::Century).is_empty());
    // module path as documented
    let _: kala_core::tick::Precision = Precision::Decade;
}

#[test]
fn log_format_version_is_six() {
    assert_eq!(LOG_FORMAT_VERSION, 6);
}

// ================================================================ defaults

#[test]
fn plain_assert_and_retract_write_exact() {
    let (mut w, g, _) = mem();
    let a = w.assert(g, "name", s("Greg"), t(1)).unwrap();
    let r = w.retract(g, "name", s("Greg"), t(5)).unwrap();
    assert_eq!(
        hist(&w, g, "name"),
        vec![
            (1, Op::Assert, s("Greg"), Precision::Exact),
            (5, Op::Retract, s("Greg"), Precision::Exact),
        ]
    );
    assert_eq!(body_precision(&w, entry(&a, 0)), Precision::Exact);
    assert_eq!(body_precision(&w, entry(&r, 0)), Precision::Exact);
    assert_eq!(
        detail(&w, g, "name", 3),
        vec![(1, s("Greg"), Precision::Exact)]
    );
    assert!(detail(&w, g, "name", 6).is_empty());
}

#[test]
fn plain_txn_writes_write_exact() {
    let (mut w, g, _) = mem();
    let mut tx = w.begin();
    tx.assert(g, "name", s("Greg"), t(1)).unwrap();
    tx.retract(g, "name", s("Greg"), t(5)).unwrap();
    let c = tx.commit().unwrap();
    for id in c.entries {
        assert_eq!(body_precision(&w, id), Precision::Exact);
    }
    assert!(hist(&w, g, "name").iter().all(|r| r.3 == Precision::Exact));
}

// ================================================================ round trips

#[test]
fn every_precision_round_trips_through_history_as_of_detail_and_entry() {
    let (mut w, g, _) = mem();
    let mut ids = vec![];
    for (i, p) in ALL.into_iter().enumerate() {
        let r = w
            .assert_with_precision(g, "name", s(&format!("n{i}")), t(10 * i as i64), p)
            .unwrap();
        assert_eq!(r.changed, 1);
        assert_eq!(r.tick, Tick(10 * i as i64));
        ids.push(entry(&r, 0));
    }
    for (i, p) in ALL.into_iter().enumerate() {
        assert_eq!(body_precision(&w, ids[i]), p, "entry {i}");
        assert_eq!(
            hist(&w, g, "name")[i],
            (10 * i as i64, Op::Assert, s(&format!("n{i}")), p)
        );
    }
    // all six hold at tick 100: sorted by value encoding ("n0" < "n1" < ...)
    let d = detail(&w, g, "name", 100);
    assert_eq!(d.len(), 6);
    for (i, p) in ALL.into_iter().enumerate() {
        assert_eq!(d[i], (10 * i as i64, s(&format!("n{i}")), p));
    }
    // earlier tick: only the values held then
    assert_eq!(
        detail(&w, g, "name", 25)
            .iter()
            .map(|r| r.2)
            .collect::<Vec<_>>(),
        vec![Precision::Exact, Precision::Day, Precision::Month]
    );
    clean(&w);
}

#[test]
fn precision_survives_the_log_and_a_reopen_and_the_frame_carries_the_byte_after_the_op() {
    let (_d, path, mut w, g, v) = file();
    let mut ids = vec![];
    for (i, p) in ALL.into_iter().enumerate() {
        let r = w
            .assert_with_precision(g, "name", s(&format!("n{i}")), t(i as i64), p)
            .unwrap();
        ids.push(entry(&r, 0));
    }
    let rr = w
        .assert_with_precision(g, "located_in", Value::Ref(v), t(7), Precision::Decade)
        .unwrap();
    let rt = w
        .retract_with_precision(g, "located_in", Value::Ref(v), t(9), Precision::Century)
        .unwrap();
    let before_h = (hist(&w, g, "name"), hist(&w, g, "located_in"));
    let before_d = detail(&w, g, "name", 50);
    // read_log
    let log = read_log(&path).unwrap();
    let fact_precisions: Vec<Precision> = log
        .iter()
        .filter_map(|e| match &e.body {
            EntryBody::Fact { precision, .. } => Some(*precision),
            _ => None,
        })
        .collect();
    let mut want: Vec<Precision> = ALL.to_vec();
    want.extend([Precision::Decade, Precision::Century]);
    assert_eq!(fact_precisions, want);
    // the byte layout: precision u8 right after the op byte, numbered 0..=5
    let bytes = std::fs::read(&path).unwrap();
    let mut seen = vec![];
    for sp in common::spans(&bytes) {
        for (fs, fe, kind) in common::frame_spans(&bytes, sp) {
            if kind == 4 {
                let payload = &bytes[fs + 4..fe];
                // kind, entry id, entity, attr, tick, op, precision, value
                let at = 1 + 8 + 8 + 4 + 8;
                seen.push((payload[at], payload[at + 1]));
            }
        }
    }
    assert_eq!(
        seen,
        vec![
            (0, 0),
            (0, 1),
            (0, 2),
            (0, 3),
            (0, 4),
            (0, 5),
            (0, 4), // located_in assert, Decade
            (1, 5), // located_in retract, Century
        ]
    );
    drop(w);
    assert!(
        doctor_log(&path, &DoctorOptions::default())
            .unwrap()
            .is_clean()
    );
    let w2 = World::open(&path).unwrap();
    assert_eq!(*w2.recovery(), RecoveryReport::default());
    assert_eq!((hist(&w2, g, "name"), hist(&w2, g, "located_in")), before_h);
    assert_eq!(detail(&w2, g, "name", 50), before_d);
    assert_eq!(body_precision(&w2, ids[3]), Precision::Year);
    assert_eq!(body_precision(&w2, entry(&rr, 0)), Precision::Decade);
    assert_eq!(body_precision(&w2, entry(&rt, 0)), Precision::Century);
    clean(&w2);
}

#[test]
fn retract_with_precision_records_the_retracts_own_precision() {
    let (mut w, g, _) = mem();
    w.assert_with_precision(g, "name", s("Greg"), t(1), Precision::Day)
        .unwrap();
    let r = w
        .retract_with_precision(g, "name", s("Greg"), t(5), Precision::Year)
        .unwrap();
    assert_eq!(r.changed, 1);
    assert_eq!(body_precision(&w, entry(&r, 0)), Precision::Year);
    assert_eq!(
        hist(&w, g, "name"),
        vec![
            (1, Op::Assert, s("Greg"), Precision::Day),
            (5, Op::Retract, s("Greg"), Precision::Year),
        ]
    );
    assert_eq!(
        detail(&w, g, "name", 3),
        vec![(1, s("Greg"), Precision::Day)]
    );
    assert!(detail(&w, g, "name", 5).is_empty());
}

// ================================================================ as_of_detail

#[test]
fn as_of_detail_reports_the_latest_assert_at_or_before_t_per_holding_value() {
    let (mut w, g, _) = mem();
    // "b" asserted first, "a" second: result is sorted by value encoding, not write order
    w.assert_with_precision(g, "name", s("b"), t(1), Precision::Day)
        .unwrap();
    w.assert_with_precision(g, "name", s("a"), t(2), Precision::Month)
        .unwrap();
    w.retract_with_precision(g, "name", s("b"), t(5), Precision::Year)
        .unwrap();
    w.assert_with_precision(g, "name", s("b"), t(9), Precision::Decade)
        .unwrap();
    assert!(detail(&w, g, "name", 0).is_empty(), "before the first fact");
    assert!(detail(&w, g, "name", i64::MIN).is_empty());
    assert_eq!(detail(&w, g, "name", 1), vec![(1, s("b"), Precision::Day)]);
    assert_eq!(
        detail(&w, g, "name", 3),
        vec![(2, s("a"), Precision::Month), (1, s("b"), Precision::Day)]
    );
    assert_eq!(
        detail(&w, g, "name", 5),
        vec![(2, s("a"), Precision::Month)],
        "retracted b is absent from its retract tick on"
    );
    assert_eq!(
        detail(&w, g, "name", 9),
        vec![
            (2, s("a"), Precision::Month),
            (9, s("b"), Precision::Decade)
        ]
    );
    assert_eq!(detail(&w, g, "name", i64::MAX).len(), 2);
}

#[test]
fn as_of_detail_values_always_equal_as_of_and_the_view_reads_an_older_canon() {
    let (mut w, g, _) = mem();
    let mut canons = vec![w.latest_tx()];
    for (i, p) in ALL.into_iter().enumerate() {
        w.assert_with_precision(g, "name", s(&format!("n{i}")), t(i as i64), p)
            .unwrap();
        canons.push(w.latest_tx());
        w.retract_with_precision(g, "name", s(&format!("n{}", i / 2)), t(10 + i as i64), p)
            .unwrap_or_else(|e| panic!("{e:?}"));
        canons.push(w.latest_tx());
    }
    for at in [i64::MIN, -1, 0, 1, 3, 5, 9, 10, 12, 20, i64::MAX] {
        let vals: Vec<Value> = w
            .as_of_detail(g, "name", Tick(at))
            .unwrap()
            .into_iter()
            .map(|c| c.value)
            .collect();
        assert_eq!(vals, w.as_of(g, "name", Tick(at)).unwrap(), "at {at}");
        for &c in &canons {
            let v = w.at_canon(c);
            let vals: Vec<Value> = v
                .as_of_detail(g, "name", Tick(at))
                .unwrap()
                .into_iter()
                .map(|c| c.value)
                .collect();
            assert_eq!(vals, v.as_of(g, "name", Tick(at)).unwrap(), "{c:?} at {at}");
        }
    }
    // an older canon shows only what was written by then
    assert!(
        w.at_canon(canons[0])
            .as_of_detail(g, "name", Tick(0))
            .unwrap()
            .is_empty()
    );
}

#[test]
fn as_of_detail_errors_like_as_of() {
    let (w, g, _) = mem();
    assert_eq!(
        w.as_of_detail(g, "nope", Tick(0)),
        Err(Error::UnknownAttribute("nope".into()))
    );
}

#[test]
fn as_of_detail_works_on_ref_attributes_and_an_edge_is_unaffected() {
    let (mut w, g, v) = mem();
    w.assert_with_precision(g, "located_in", Value::Ref(v), t(-300), Precision::Century)
        .unwrap();
    assert_eq!(
        detail(&w, g, "located_in", 0),
        vec![(-300, Value::Ref(v), Precision::Century)]
    );
    assert_eq!(
        w.neighbors(g, "located_in", Direction::Out, Tick(0))
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        w.lookup("located_in", &Value::Ref(v), Tick(0)).unwrap(),
        vec![g]
    );
}

// ================================================================ corrections

#[test]
fn a_precision_only_replace_is_a_same_key_revised_with_the_op_unchanged() {
    let (mut w, g, _) = mem();
    let base = w
        .assert_with_precision(g, "name", s("Greg"), t(5), Precision::Day)
        .unwrap();
    let old_canon = w.latest_tx();
    let r = w
        .correct(
            entry(&base, 0),
            rep(None, None, None, Some(Precision::Year)),
        )
        .unwrap();
    assert_eq!(r.changed, 1);
    assert_eq!(r.tick, Tick(5));
    assert_eq!(r.entries.len(), 1, "a same-key Revised only (no Deleted)");
    let info = w.entry(entry(&r, 0)).unwrap();
    assert_eq!(info.revision(), Some((entry(&base, 0), 1)));
    match info.body {
        EntryBody::Revised {
            tick,
            op,
            precision,
            ref value,
            ..
        } => {
            assert_eq!(
                (tick, op, precision),
                (Tick(5), Op::Assert, Precision::Year)
            );
            assert_eq!(*value, s("Greg"));
        }
        ref other => panic!("{other:?}"),
    }
    // the current canon says Year, an older canon still says Day
    assert_eq!(
        hist(&w, g, "name"),
        vec![(5, Op::Assert, s("Greg"), Precision::Year)]
    );
    assert_eq!(
        detail(&w, g, "name", 5),
        vec![(5, s("Greg"), Precision::Year)]
    );
    let old = w.at_canon(old_canon);
    let oh: Vec<_> = old
        .history(g, "name")
        .unwrap()
        .into_iter()
        .map(|c| (c.tick.0, c.op, c.precision))
        .collect();
    assert_eq!(oh, vec![(5, Op::Assert, Precision::Day)]);
    assert_eq!(
        old.as_of_detail(g, "name", Tick(5)).unwrap()[0].precision,
        Precision::Day
    );
    // the same through the main (non-latest) indexes
    let main = w.at_canon_main(w.latest_tx());
    assert_eq!(
        main.as_of_detail(g, "name", Tick(5)).unwrap()[0].precision,
        Precision::Year
    );
    // answers about the fact itself are untouched
    assert_eq!(w.as_of(g, "name", Tick(5)).unwrap(), vec![s("Greg")]);
    assert_eq!(w.lookup("name", &s("Greg"), Tick(5)).unwrap(), vec![g]);
    clean(&w);
}

#[test]
fn a_precision_only_replace_of_a_retract_changes_the_retracts_precision() {
    let (mut w, g, _) = mem();
    w.assert(g, "name", s("Greg"), t(1)).unwrap();
    let rt = w.retract(g, "name", s("Greg"), t(5)).unwrap();
    let r = w
        .correct(entry(&rt, 0), rep(None, None, None, Some(Precision::Month)))
        .unwrap();
    assert_eq!(r.changed, 1);
    assert_eq!(
        hist(&w, g, "name"),
        vec![
            (1, Op::Assert, s("Greg"), Precision::Exact),
            (5, Op::Retract, s("Greg"), Precision::Month),
        ]
    );
    assert!(detail(&w, g, "name", 5).is_empty(), "still retracted at 5");
    clean(&w);
}

#[test]
fn successive_precision_replaces_chain_revisions_and_can_return_to_exact() {
    let (mut w, g, _) = mem();
    let base = w
        .assert_with_precision(g, "name", s("Greg"), t(5), Precision::Day)
        .unwrap();
    let id = entry(&base, 0);
    for (i, p) in [Precision::Year, Precision::Exact, Precision::Century]
        .into_iter()
        .enumerate()
    {
        let r = w.correct(id, rep(None, None, None, Some(p))).unwrap();
        assert_eq!(r.changed, 1, "step {i}");
        assert_eq!(
            w.entry(entry(&r, 0)).unwrap().revision(),
            Some((id, i as u32 + 1))
        );
        assert_eq!(hist(&w, g, "name")[0].3, p);
    }
    assert_eq!(hist(&w, g, "name").len(), 1);
    clean(&w);
}

#[test]
fn a_replace_that_changes_the_tick_keeps_the_precision_unless_given() {
    let (mut w, g, _) = mem();
    let base = w
        .assert_with_precision(g, "name", s("Greg"), t(1), Precision::Month)
        .unwrap();
    let r = w
        .correct(entry(&base, 0), rep(None, t(5), None, None))
        .unwrap();
    assert_eq!(r.changed, 2);
    assert_eq!(r.tick, Tick(5));
    assert_eq!(r.entries.len(), 2, "Deleted + Revised");
    assert_eq!(body_precision(&w, entry(&r, 1)), Precision::Month);
    assert_eq!(
        hist(&w, g, "name"),
        vec![(5, Op::Assert, s("Greg"), Precision::Month)]
    );
    clean(&w);
}

#[test]
fn a_replace_that_changes_the_value_or_op_keeps_the_precision_unless_given() {
    let (mut w, g, _) = mem();
    let a = w
        .assert_with_precision(g, "name", s("Greg"), t(1), Precision::Year)
        .unwrap();
    // value change (a move to another key)
    let r = w
        .correct(entry(&a, 0), rep(Some(s("Gregor")), None, None, None))
        .unwrap();
    assert_eq!(r.changed, 2);
    assert_eq!(
        hist(&w, g, "name"),
        vec![(1, Op::Assert, s("Gregor"), Precision::Year)]
    );
    // op change at the same key (assert -> retract) keeps it too
    let rr = w
        .correct(entry(&a, 0), rep(None, None, Some(Op::Retract), None))
        .unwrap();
    assert_eq!(rr.changed, 1);
    assert_eq!(
        hist(&w, g, "name"),
        vec![(1, Op::Retract, s("Gregor"), Precision::Year)]
    );
    clean(&w);
}

#[test]
fn a_replace_changing_the_tick_and_the_precision_writes_both() {
    let (mut w, g, _) = mem();
    let base = w
        .assert_with_precision(g, "name", s("Greg"), t(1), Precision::Day)
        .unwrap();
    let r = w
        .correct(
            entry(&base, 0),
            rep(None, t(-300), None, Some(Precision::Century)),
        )
        .unwrap();
    assert_eq!(r.changed, 2);
    assert_eq!(r.tick, Tick(-300));
    let deleted = w.entry(entry(&r, 0)).unwrap();
    let revised = w.entry(entry(&r, 1)).unwrap();
    assert!(matches!(
        deleted.body,
        EntryBody::Deleted { tick: Tick(1), .. }
    ));
    assert_eq!(deleted.revision(), revised.revision());
    assert_eq!(body_precision(&w, entry(&r, 1)), Precision::Century);
    assert_eq!(
        hist(&w, g, "name"),
        vec![(-300, Op::Assert, s("Greg"), Precision::Century)]
    );
    assert_eq!(
        detail(&w, g, "name", 0),
        vec![(-300, s("Greg"), Precision::Century)]
    );
    clean(&w);
}

#[test]
fn a_replace_that_changes_op_and_precision_at_once_is_one_same_key_revised() {
    let (mut w, g, _) = mem();
    w.assert(g, "name", s("Greg"), t(1)).unwrap();
    let rt = w
        .retract_with_precision(g, "name", s("Greg"), t(5), Precision::Day)
        .unwrap();
    let r = w
        .correct(
            entry(&rt, 0),
            rep(None, None, Some(Op::Assert), Some(Precision::Decade)),
        )
        .unwrap();
    assert_eq!((r.changed, r.entries.len()), (1, 1));
    assert_eq!(
        hist(&w, g, "name")[1],
        (5, Op::Assert, s("Greg"), Precision::Decade)
    );
    clean(&w);
}

#[test]
fn a_replace_that_differs_in_nothing_including_precision_is_zero_changed() {
    let (mut w, g, _) = mem();
    let base = w
        .assert_with_precision(g, "name", s("Greg"), t(5), Precision::Day)
        .unwrap();
    let tx = w.latest_tx();
    for c in [
        rep(None, None, None, None),
        rep(None, None, None, Some(Precision::Day)),
        rep(
            Some(s("Greg")),
            t(5),
            Some(Op::Assert),
            Some(Precision::Day),
        ),
    ] {
        let r = w.correct(entry(&base, 0), c.clone()).unwrap();
        assert_eq!(r.changed, 0, "{c:?}");
        assert!(r.entries.is_empty(), "{c:?}");
        assert_eq!(r.tick, Tick(5));
        assert_eq!(w.latest_tx(), tx, "{c:?}: nothing appended");
    }
    assert_eq!(hist(&w, g, "name")[0].3, Precision::Day);
}

#[test]
fn a_precision_only_replace_that_matches_the_current_revision_is_a_noop_but_one_that_differs_from_it_is_not()
 {
    let (mut w, g, _) = mem();
    let base = w
        .assert_with_precision(g, "name", s("Greg"), t(5), Precision::Day)
        .unwrap();
    let id = entry(&base, 0);
    w.correct(id, rep(None, None, None, Some(Precision::Year)))
        .unwrap();
    // the base entry said Day, the current revision says Year: Some(Year) is a no-op now
    let r = w
        .correct(id, rep(None, None, None, Some(Precision::Year)))
        .unwrap();
    assert_eq!(r.changed, 0);
    // and Some(Day) (the base's value) is a real change back
    let r = w
        .correct(id, rep(None, None, None, Some(Precision::Day)))
        .unwrap();
    assert_eq!(r.changed, 1);
    assert_eq!(hist(&w, g, "name")[0].3, Precision::Day);
    clean(&w);
}

#[test]
fn replace_precision_inside_a_txn_and_a_second_correction_of_the_chain_conflicts() {
    let (mut w, g, _) = mem();
    let base = w
        .assert_with_precision(g, "name", s("Greg"), t(5), Precision::Day)
        .unwrap();
    let id = entry(&base, 0);
    {
        let mut tx = w.begin();
        let r = tx
            .correct(id, rep(None, None, None, Some(Precision::Month)))
            .unwrap();
        assert_eq!(r.changed, 1);
        let c = tx.commit().unwrap();
        assert_eq!(c.entries.len(), 1);
    }
    assert_eq!(hist(&w, g, "name")[0].3, Precision::Month);
    let mut tx = w.begin();
    tx.correct(id, rep(None, None, None, Some(Precision::Year)))
        .unwrap();
    let err = tx
        .correct(id, rep(None, None, None, Some(Precision::Century)))
        .unwrap_err();
    assert!(matches!(err, Error::TxnKeyConflict { .. }), "{err:?}");
    drop(tx);
    assert_eq!(hist(&w, g, "name")[0].3, Precision::Month);
}

#[test]
fn delete_of_a_fact_with_precision_still_removes_it_and_older_canon_keeps_its_precision() {
    let (mut w, g, _) = mem();
    let a = w
        .assert_with_precision(g, "name", s("Greg"), t(5), Precision::Decade)
        .unwrap();
    let before = w.latest_tx();
    w.correct(entry(&a, 0), Correction::Delete).unwrap();
    assert!(hist(&w, g, "name").is_empty());
    assert!(detail(&w, g, "name", 9).is_empty());
    assert_eq!(
        w.at_canon(before).as_of_detail(g, "name", Tick(9)).unwrap()[0].precision,
        Precision::Decade
    );
    clean(&w);
}

// ================================================================ no-op / conflict rules

#[test]
fn asserting_a_held_value_with_another_precision_is_a_noop_that_records_nothing() {
    let (mut w, g, _) = mem();
    w.assert_with_precision(g, "name", s("Greg"), t(1), Precision::Day)
        .unwrap();
    let tx = w.latest_tx();
    let h = hist(&w, g, "name");
    // held at tick 1 (same tick), and at a later tick
    for at in [1, 7] {
        let r = w
            .assert_with_precision(g, "name", s("Greg"), t(at), Precision::Century)
            .unwrap();
        assert_eq!(r.changed, 0, "at {at}");
        assert!(r.entries.is_empty());
        assert_eq!(r.tick, Tick(at));
        assert_eq!(w.latest_tx(), tx, "nothing appended");
    }
    assert_eq!(hist(&w, g, "name"), h);
    assert_eq!(
        detail(&w, g, "name", 9),
        vec![(1, s("Greg"), Precision::Day)]
    );
}

#[test]
fn retracting_an_unheld_value_with_a_precision_is_a_noop() {
    let (mut w, g, _) = mem();
    let tx = w.latest_tx();
    let r = w
        .retract_with_precision(g, "name", s("Greg"), t(5), Precision::Year)
        .unwrap();
    assert_eq!(r.changed, 0);
    assert!(r.entries.is_empty());
    assert_eq!(w.latest_tx(), tx);
    // retracting before the assert at the write's tick is a noop; after, it is stored
    w.assert(g, "name", s("Greg"), t(10)).unwrap();
    assert_eq!(
        w.retract_with_precision(g, "name", s("Greg"), t(3), Precision::Year)
            .unwrap()
            .changed,
        0
    );
    assert_eq!(
        w.retract_with_precision(g, "name", s("Greg"), t(11), Precision::Year)
            .unwrap()
            .changed,
        1
    );
    // a second retract of the same value at the same tick, other precision: no-op
    assert_eq!(
        w.retract_with_precision(g, "name", s("Greg"), t(11), Precision::Day)
            .unwrap()
            .changed,
        0
    );
    assert_eq!(hist(&w, g, "name")[1].3, Precision::Year);
}

#[test]
fn repeating_the_same_op_at_the_same_tick_with_another_precision_is_a_noop() {
    let (mut w, g, _) = mem();
    w.assert_with_precision(g, "name", s("Greg"), t(1), Precision::Day)
        .unwrap();
    let r = w
        .assert_with_precision(g, "name", s("Greg"), t(1), Precision::Year)
        .unwrap();
    assert_eq!(r.changed, 0);
    assert_eq!(hist(&w, g, "name")[0].3, Precision::Day);
}

#[test]
fn the_opposite_op_at_the_same_tick_is_a_same_tick_conflict_whatever_the_precision() {
    let (mut w, g, _) = mem();
    w.assert_with_precision(g, "name", s("Greg"), t(1), Precision::Day)
        .unwrap();
    let tx = w.latest_tx();
    for p in ALL {
        let err = w
            .retract_with_precision(g, "name", s("Greg"), t(1), p)
            .unwrap_err();
        assert_eq!(
            err,
            Error::SameTickConflict {
                entity: g,
                attr: "name".into(),
                tick: Tick(1)
            },
            "{p:?}"
        );
    }
    assert_eq!(w.latest_tx(), tx);
    // and the other way round
    w.retract_with_precision(g, "name", s("Greg"), t(4), Precision::Year)
        .unwrap();
    for p in ALL {
        assert!(matches!(
            w.assert_with_precision(g, "name", s("Greg"), t(4), p),
            Err(Error::SameTickConflict { .. })
        ));
    }
}

#[test]
fn within_one_txn_the_first_write_of_a_key_wins_and_the_second_is_a_noop() {
    let (mut w, g, _) = mem();
    let mut tx = w.begin();
    assert_eq!(
        tx.assert_with_precision(g, "name", s("Greg"), t(1), Precision::Day)
            .unwrap()
            .changed,
        1
    );
    assert_eq!(
        tx.assert_with_precision(g, "name", s("Greg"), t(1), Precision::Year)
            .unwrap()
            .changed,
        0
    );
    let err = tx
        .retract_with_precision(g, "name", s("Greg"), t(1), Precision::Day)
        .unwrap_err();
    assert!(matches!(err, Error::SameTickConflict { .. }), "{err:?}");
    drop(tx);
    assert!(
        hist(&w, g, "name").is_empty(),
        "the poisoned txn applied nothing"
    );
}

#[test]
fn write_errors_are_unchanged_by_a_precision() {
    let (mut w, g, v) = mem();
    assert_eq!(
        w.assert_with_precision(g, "nope", s("x"), t(1), Precision::Day),
        Err(Error::UnknownAttribute("nope".into()))
    );
    assert!(matches!(
        w.assert_with_precision(g, "name", Value::I64(1), t(1), Precision::Day),
        Err(Error::WrongValueType { .. })
    ));
    assert_eq!(
        w.retract_with_precision(g, "bought", Value::Ref(v), t(1), Precision::Year),
        Err(Error::RetractEvent("bought".into()))
    );
    // an event may be asserted with a precision
    let r = w
        .assert_with_precision(g, "bought", Value::Ref(v), t(1), Precision::Year)
        .unwrap();
    assert_eq!(body_precision(&w, entry(&r, 0)), Precision::Year);
    assert_eq!(w.latest_tx(), TxId(3));
}

// ================================================================ ticks and defaults

#[test]
fn a_precision_write_with_no_tick_uses_the_latest_tick_and_reports_it() {
    let (mut w, g, _) = mem();
    // an empty world's latest tick is 0
    let r = w
        .assert_with_precision(g, "name", s("a"), None, Precision::Year)
        .unwrap();
    assert_eq!(r.tick, Tick(0));
    w.assert(g, "name", s("b"), t(601)).unwrap();
    let r = w
        .assert_with_precision(g, "name", s("c"), None, Precision::Decade)
        .unwrap();
    assert_eq!(r.tick, Tick(601));
    let h = hist(&w, g, "name");
    assert!(h.contains(&(0, Op::Assert, s("a"), Precision::Year)));
    assert!(h.contains(&(601, Op::Assert, s("c"), Precision::Decade)));
    // a retract with no tick lands on the latest tick too
    let r = w
        .retract_with_precision(g, "name", s("c"), None, Precision::Month)
        .unwrap_err();
    assert!(
        matches!(r, Error::SameTickConflict { .. }),
        "assert then retract of one value at the latest tick conflicts: {r:?}"
    );
}

#[test]
fn txn_precision_writes_use_the_txn_default_then_the_latest_tick_at_begin() {
    let (mut w, g, _) = mem();
    w.assert(g, "name", s("seed"), t(100)).unwrap();
    let mut tx = w.begin();
    // no default: the latest tick at begin (100), for every tick-less write
    let a = tx
        .assert_with_precision(g, "name", s("a"), None, Precision::Month)
        .unwrap();
    // a write with a later tick doesn't move it
    tx.assert(g, "name", s("far"), t(900)).unwrap();
    let b = tx
        .assert_with_precision(g, "name", s("b"), None, Precision::Year)
        .unwrap();
    assert_eq!((a.tick, b.tick), (Tick(100), Tick(100)));
    // a default wins over the latest tick; a write's own tick wins over both
    tx.set_default_tick(Tick(-208));
    let c = tx
        .assert_with_precision(g, "name", s("c"), None, Precision::Day)
        .unwrap();
    let d = tx
        .assert_with_precision(g, "name", s("d"), t(-100), Precision::Decade)
        .unwrap();
    assert_eq!((c.tick, d.tick), (Tick(-208), Tick(-100)));
    // retract at the default tick of a value asserted there: a same-tick conflict
    let err = tx
        .retract_with_precision(g, "name", s("c"), None, Precision::Century)
        .unwrap_err();
    assert!(matches!(err, Error::SameTickConflict { .. }), "{err:?}");
    drop(tx);

    // the same, committed
    let mut tx = w.begin();
    tx.set_default_tick(Tick(-208));
    tx.assert_with_precision(g, "name", s("c"), None, Precision::Day)
        .unwrap();
    tx.assert_with_precision(g, "name", s("e"), t(7), Precision::Century)
        .unwrap();
    tx.retract_with_precision(g, "name", s("c"), t(8), Precision::Year)
        .unwrap();
    let c = tx.commit().unwrap();
    assert_eq!(c.changed, 3);
    let h = hist(&w, g, "name");
    assert!(h.contains(&(-208, Op::Assert, s("c"), Precision::Day)));
    assert!(h.contains(&(7, Op::Assert, s("e"), Precision::Century)));
    assert!(h.contains(&(8, Op::Retract, s("c"), Precision::Year)));
    for (id, p) in c
        .entries
        .iter()
        .zip([Precision::Day, Precision::Century, Precision::Year])
    {
        assert_eq!(body_precision(&w, *id), p);
    }
    clean(&w);
}

// ================================================================ keys and indexes ignore precision

#[test]
fn indexes_and_every_precision_blind_read_are_identical_with_and_without_precisions() {
    let build = |with: bool| {
        let (mut w, g, v) = mem();
        let p = |p: Precision| if with { p } else { Precision::Exact };
        w.assert_with_precision(g, "name", s("Greg"), t(1), p(Precision::Day))
            .unwrap();
        w.assert_with_precision(g, "name", s("Gregor"), t(2), p(Precision::Year))
            .unwrap();
        w.retract_with_precision(g, "name", s("Greg"), t(5), p(Precision::Month))
            .unwrap();
        w.assert_with_precision(
            g,
            "located_in",
            Value::Ref(v),
            t(-300),
            p(Precision::Century),
        )
        .unwrap();
        w.retract_with_precision(
            g,
            "located_in",
            Value::Ref(v),
            t(-250),
            p(Precision::Decade),
        )
        .unwrap();
        w.assert_with_precision(g, "located_in", Value::Ref(v), t(-200), p(Precision::Day))
            .unwrap();
        // a precision-only no-op and a precision-only correction on one side
        w.assert_with_precision(g, "name", s("Gregor"), t(3), p(Precision::Century))
            .unwrap();
        (w, g, v)
    };
    let (a, g, v) = build(true);
    let (b, _, _) = build(false);
    assert_eq!(
        a.index_dump(),
        b.index_dump(),
        "precision is never part of a key or a row"
    );
    assert_eq!(a.latest_tx(), b.latest_tx());
    for at in [i64::MIN, -300, -251, -250, -200, 0, 1, 2, 5, 6, i64::MAX] {
        let tk = Tick(at);
        assert_eq!(a.as_of(g, "name", tk), b.as_of(g, "name", tk), "at {at}");
        assert_eq!(a.as_of(g, "located_in", tk), b.as_of(g, "located_in", tk));
        assert_eq!(
            a.lookup("name", &s("Greg"), tk),
            b.lookup("name", &s("Greg"), tk)
        );
        assert_eq!(
            a.lookup_ci("name", "gregor", tk),
            b.lookup_ci("name", "gregor", tk)
        );
        assert_eq!(
            a.neighbors(g, "located_in", Direction::Out, tk),
            b.neighbors(g, "located_in", Direction::Out, tk)
        );
        assert_eq!(
            a.neighbors(v, "located_in", Direction::In, tk),
            b.neighbors(v, "located_in", Direction::In, tk)
        );
        assert_eq!(
            a.as_of_value(g, "name", &s("Greg"), tk),
            b.as_of_value(g, "name", &s("Greg"), tk)
        );
    }
    assert_eq!(a.overlaps("name"), b.overlaps("name"));
    // histories differ only in precision
    let strip = |w: &World| {
        w.history(g, "name")
            .unwrap()
            .into_iter()
            .map(|c| (c.tick, c.op, c.value))
            .collect::<Vec<_>>()
    };
    assert_eq!(strip(&a), strip(&b));
    clean(&a);
}

#[test]
fn a_precision_only_replace_adds_exactly_one_revised_row_to_each_index_and_nothing_else() {
    let (mut w, g, v) = mem();
    let n = w
        .assert_with_precision(g, "name", s("Greg"), t(1), Precision::Day)
        .unwrap();
    let l = w
        .assert_with_precision(g, "located_in", Value::Ref(v), t(1), Precision::Day)
        .unwrap();
    for (id, tick) in [(entry(&n, 0), 1), (entry(&l, 0), 1)] {
        let before = w.index_dump();
        let tx_before = w.latest_tx();
        let r = w
            .correct(id, rep(None, None, None, Some(Precision::Year)))
            .unwrap();
        assert_eq!(r.changed, 1);
        let after = w.index_dump();
        let new_tx = w.latest_tx();
        assert_eq!(new_tx.0, tx_before.0 + 1);
        // E
        assert_eq!(after.e.len(), before.e.len() + 1);
        assert!(before.e.iter().all(|row| after.e.contains(row)));
        let extra: Vec<_> = after
            .e
            .iter()
            .filter(|row| !before.e.contains(row))
            .collect();
        assert_eq!(extra.len(), 1);
        assert_eq!(
            (extra[0].3, extra[0].4, extra[0].5),
            (Tick(tick), new_tx, Mark::Assert)
        );
        assert!(
            after.e.iter().all(|row| row.5 != Mark::Deleted),
            "no DELETED marker"
        );
        // R (only the ref attribute has rows there)
        let extra_r = after.r.len() - before.r.len();
        assert!(before.r.iter().all(|row| after.r.contains(row)));
        assert!(extra_r <= 1);
        // V and VFold: only the string attribute has rows; one new Revised row each, if any
        for (b, a) in [
            (before.v.len(), after.v.len()),
            (before.v_fold.len(), after.v_fold.len()),
        ] {
            assert!(a == b || a == b + 1, "at most one new row");
        }
        assert!(before.v.iter().all(|row| after.v.contains(row)));
        assert!(before.v_fold.iter().all(|row| after.v_fold.contains(row)));
        assert!(after.v.iter().all(|row| row.5 != Mark::Deleted));
        assert!(after.v_fold.iter().all(|row| row.6 != Mark::Deleted));
    }
    // the string attribute does have V and VFold rows: exactly one more of each
    let (mut w, g, _) = mem();
    let n = w
        .assert_with_precision(g, "name", s("Greg"), t(1), Precision::Day)
        .unwrap();
    let before = w.index_dump();
    assert!(!before.v.is_empty() && !before.v_fold.is_empty());
    w.correct(entry(&n, 0), rep(None, None, None, Some(Precision::Month)))
        .unwrap();
    let after = w.index_dump();
    assert_eq!(after.v.len(), before.v.len() + 1);
    assert_eq!(after.v_fold.len(), before.v_fold.len() + 1);
    // the values answered are unchanged
    assert_eq!(w.lookup("name", &s("Greg"), Tick(1)).unwrap(), vec![g]);
    assert_eq!(w.lookup_ci("name", "greg", Tick(1)).unwrap(), vec![g]);
    clean(&w);
}

#[test]
fn doctor_is_clean_after_precision_writes_and_every_kind_of_precision_correction() {
    let (dir, path, mut w, g, v) = file();
    let _keep = &dir;
    let a = w
        .assert_with_precision(g, "name", s("Greg"), t(1), Precision::Day)
        .unwrap();
    let b = w
        .assert_with_precision(g, "located_in", Value::Ref(v), t(-300), Precision::Century)
        .unwrap();
    let c = w
        .retract_with_precision(g, "located_in", Value::Ref(v), t(-250), Precision::Decade)
        .unwrap();
    clean(&w);
    w.correct(entry(&a, 0), rep(None, None, None, Some(Precision::Year)))
        .unwrap();
    clean(&w);
    w.correct(
        entry(&b, 0),
        rep(None, t(-400), None, Some(Precision::Exact)),
    )
    .unwrap();
    clean(&w);
    w.correct(
        entry(&c, 0),
        rep(None, None, Some(Op::Assert), Some(Precision::Month)),
    )
    .unwrap();
    clean(&w);
    w.correct(
        entry(&a, 0),
        rep(Some(s("Gregor")), None, None, Some(Precision::Day)),
    )
    .unwrap();
    clean(&w);
    w.correct(entry(&a, 0), Correction::Delete).unwrap();
    clean(&w);
    let live = (hist(&w, g, "name"), hist(&w, g, "located_in"));
    drop(w);
    assert!(
        doctor_log(&path, &DoctorOptions::default())
            .unwrap()
            .is_clean()
    );
    let w = World::open(&path).unwrap();
    assert_eq!((hist(&w, g, "name"), hist(&w, g, "located_in")), live);
    clean(&w);
}

// ================================================================ crafted v4 logs

const K_FACT: u8 = 4;
const K_DELETED: u8 = 7;
const K_REVISED: u8 = 8;

struct Crafted {
    _dir: tempfile::TempDir,
    path: PathBuf,
    bytes: Vec<u8>,
    greg: u64,
    attr_name: u32,
    /// `Greg name "Greg" @1 assert`, Exact
    f_assert: u64,
    /// `Greg name "Greg" @5 retract`, Exact
    f_retract: u64,
    next_entry: u64,
    next_tx: u64,
}

fn crafted() -> Crafted {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("w.kala");
    let (g, f_assert, f_retract);
    {
        let mut w = World::create(&path, schema()).unwrap();
        g = w.create_entity("CHARACTER", Some("greg")).unwrap();
        f_assert = w.assert(g, "name", s("Greg"), t(1)).unwrap().entries[0].0;
        f_retract = w.retract(g, "name", s("Greg"), t(5)).unwrap().entries[0].0;
    }
    let log = read_log(&path).unwrap();
    let attr_name = log
        .iter()
        .find_map(|e| match &e.body {
            EntryBody::DefineAttr { id, def } if def.name == "name" => Some(id.0),
            _ => None,
        })
        .unwrap();
    let last_tx = log
        .iter()
        .filter_map(|e| match &e.body {
            EntryBody::Commit { tx, .. } => Some(tx.0),
            _ => None,
        })
        .max()
        .unwrap();
    Crafted {
        bytes: std::fs::read(&path).unwrap(),
        greg: u64::from_be_bytes(g.to_bytes()),
        attr_name,
        f_assert,
        f_retract,
        next_entry: log.last().unwrap().id.0 + 1,
        next_tx: last_tx + 1,
        path,
        _dir: dir,
    }
}

impl Crafted {
    /// A v4 Fact payload: entity | attr | tick | op | precision | value.
    fn fact(&self, tick: i64, retract: bool, precision: u8, v: &str) -> (u8, Vec<u8>) {
        let mut b = self.greg.to_le_bytes().to_vec();
        b.extend_from_slice(&self.attr_name.to_le_bytes());
        b.extend_from_slice(&tick.to_le_bytes());
        b.push(u8::from(retract));
        b.push(precision);
        b.extend_from_slice(&s(v).encode().unwrap());
        (K_FACT, b)
    }
    /// A v4 Revised payload: of | rev | entity | attr | tick | op | precision | value.
    fn revised(&self, of: u64, rev: u32, tick: i64, retract: bool, precision: u8) -> (u8, Vec<u8>) {
        let mut b = of.to_le_bytes().to_vec();
        b.extend_from_slice(&rev.to_le_bytes());
        b.extend_from_slice(&self.greg.to_le_bytes());
        b.extend_from_slice(&self.attr_name.to_le_bytes());
        b.extend_from_slice(&tick.to_le_bytes());
        b.push(u8::from(retract));
        b.push(precision);
        b.extend_from_slice(&s("Greg").encode().unwrap());
        (K_REVISED, b)
    }
    fn deleted(&self, of: u64, rev: u32, tick: i64) -> (u8, Vec<u8>) {
        let mut b = of.to_le_bytes().to_vec();
        b.extend_from_slice(&rev.to_le_bytes());
        b.extend_from_slice(&self.greg.to_le_bytes());
        b.extend_from_slice(&self.attr_name.to_le_bytes());
        b.extend_from_slice(&tick.to_le_bytes());
        b.extend_from_slice(&s("Greg").encode().unwrap());
        (K_DELETED, b)
    }
    /// Appends one valid txn made of `entries` to the crafted log for good.
    fn push(&mut self, entries: &[(u8, Vec<u8>)]) {
        self.bytes = self.with(entries);
        self.next_entry += entries.len() as u64 + 1;
        self.next_tx += 1;
    }
    /// The file with one more valid txn made of `entries` (the crafted log itself is unchanged).
    fn with(&self, entries: &[(u8, Vec<u8>)]) -> Vec<u8> {
        let mut l = self.bytes.clone();
        let mut body = Vec::new();
        let mut id = self.next_entry;
        for (k, p) in entries {
            body.extend(frame(*k, id, p));
            id += 1;
        }
        body.extend(commit_frame(
            common::link(&l),
            &body.clone(),
            id,
            self.next_tx,
        ));
        l.extend(txn(self.next_tx, &body));
        l
    }
    fn open(&self, file: &[u8]) -> Result<World, Error> {
        std::fs::write(&self.path, file).unwrap();
        World::open(&self.path)
    }
    /// `World::open` says Corrupt and leaves the file alone (replay rules; `read_log` only
    /// checks framing and parsing, so it is not asserted here).
    fn corrupt_open(&self, file: &[u8], label: &str) {
        match self.open(file) {
            Err(Error::Corrupt(m)) => eprintln!("{label}: {m}"),
            Err(e) => panic!("{label}: expected Corrupt, got {e:?}"),
            Ok(_) => panic!("{label}: expected Corrupt, open succeeded"),
        }
        assert_eq!(
            std::fs::read(&self.path).unwrap(),
            file,
            "{label}: file touched"
        );
    }
    /// Byte-level damage: `World::open` and `read_log` both say Corrupt.
    fn corrupt(&self, file: &[u8], label: &str) {
        self.corrupt_open(file, label);
        assert!(
            matches!(read_log(&self.path), Err(Error::Corrupt(_))),
            "{label}: read_log must say Corrupt too"
        );
    }
    fn ok(&self, file: &[u8], label: &str) -> World {
        match self.open(file) {
            Ok(w) => w,
            Err(e) => panic!("{label}: expected a clean open, got {e:?}"),
        }
    }
}

#[test]
fn a_fact_with_a_precision_byte_above_five_is_corrupt_and_zero_to_five_open() {
    let c = crafted();
    for p in 0..=5u8 {
        let w = c.ok(
            &c.with(&[c.fact(9, false, p, "Gregor")]),
            &format!("precision {p}"),
        );
        let e = w.entity_by_handle("greg").unwrap();
        assert_eq!(
            hist(&w, e, "name").last().unwrap().3,
            ALL[p as usize],
            "byte {p}"
        );
    }
    for p in [6u8, 7, 0x80, 0xFF] {
        c.corrupt(
            &c.with(&[c.fact(9, false, p, "Gregor")]),
            &format!("fact precision {p}"),
        );
    }
}

#[test]
fn a_revised_with_a_precision_byte_above_five_is_corrupt() {
    let c = crafted();
    // control: a valid precision-only revision opens
    let w = c.ok(&c.with(&[c.revised(c.f_assert, 1, 1, false, 5)]), "control");
    let e = w.entity_by_handle("greg").unwrap();
    assert_eq!(hist(&w, e, "name")[0].3, Precision::Century);
    drop(w); // release the file lock before the next open
    for p in [6u8, 9, 0xFF] {
        c.corrupt(
            &c.with(&[c.revised(c.f_assert, 1, 1, false, p)]),
            &format!("revised precision {p}"),
        );
        // moved: Deleted + Revised, the Revised carries the bad byte
        c.corrupt(
            &c.with(&[
                c.deleted(c.f_assert, 1, 1),
                c.revised(c.f_assert, 1, 2, false, p),
            ]),
            &format!("moved revised precision {p}"),
        );
    }
}

#[test]
fn a_same_key_revised_that_changes_neither_op_nor_precision_is_corrupt() {
    let c = crafted();
    // base f_assert is an assert, Exact: unchanged -> Corrupt
    c.corrupt_open(
        &c.with(&[c.revised(c.f_assert, 1, 1, false, 0)]),
        "unchanged vs an Exact assert",
    );
    c.corrupt_open(
        &c.with(&[c.revised(c.f_retract, 1, 5, true, 0)]),
        "unchanged vs an Exact retract",
    );
    // controls: changing only the precision, only the op, or both opens
    for (label, e) in [
        ("precision only", c.revised(c.f_assert, 1, 1, false, 3)),
        ("op only", c.revised(c.f_assert, 1, 1, true, 0)),
        ("op and precision", c.revised(c.f_assert, 1, 1, true, 3)),
    ] {
        let w = c.ok(&c.with(&[e]), label);
        clean(&w);
    }
}

#[test]
fn a_same_key_revised_is_judged_against_the_current_revision_not_the_base() {
    let mut c = crafted();
    c.push(&[c.revised(c.f_assert, 1, 1, false, 3)]);
    let rev1_tx = c.next_tx - 1;
    // rev 2 repeating rev 1's precision (and op) changes nothing relative to the current entry
    c.corrupt_open(
        &c.with(&[c.revised(c.f_assert, 2, 1, false, 3)]),
        "rev 2 equals rev 1",
    );
    // rev 2 returning to the base's precision is a real change (vs rev 1) and opens
    let w = c.ok(
        &c.with(&[c.revised(c.f_assert, 2, 1, false, 0)]),
        "rev 2 back to Exact",
    );
    let e = w.entity_by_handle("greg").unwrap();
    assert_eq!(hist(&w, e, "name")[0].3, Precision::Exact);
    // and an older canon sees rev 1's precision (Year)
    let older = w.at_canon(TxId(rev1_tx));
    assert_eq!(
        older.history(e, "name").unwrap()[0].precision,
        Precision::Year
    );
}

#[test]
fn a_moved_revised_may_keep_the_same_precision_because_the_key_changed() {
    let c = crafted();
    // Deleted at the old key + Revised at a new tick with the same op and precision: valid
    let w = c.ok(
        &c.with(&[
            c.deleted(c.f_assert, 1, 1),
            c.revised(c.f_assert, 1, 2, false, 0),
        ]),
        "moved, nothing else changed",
    );
    let e = w.entity_by_handle("greg").unwrap();
    assert_eq!(
        hist(&w, e, "name")[0],
        (2, Op::Assert, s("Greg"), Precision::Exact)
    );
    clean(&w);
}

#[test]
fn replayed_precision_equals_written_precision_for_every_entry_kind() {
    let c = crafted();
    let file = c.with(&[
        c.fact(20, false, 2, "Gregor"),
        c.revised(c.f_assert, 1, 1, false, 4),
    ]);
    let w = c.ok(&file, "mixed");
    let e = w.entity_by_handle("greg").unwrap();
    assert_eq!(
        hist(&w, e, "name"),
        vec![
            (1, Op::Assert, s("Greg"), Precision::Decade),
            (5, Op::Retract, s("Greg"), Precision::Exact),
            (20, Op::Assert, s("Gregor"), Precision::Month),
        ]
    );
    let log = read_log(&c.path).unwrap();
    let ps: Vec<Precision> = log
        .iter()
        .filter_map(|e| match &e.body {
            EntryBody::Fact { precision, .. } | EntryBody::Revised { precision, .. } => {
                Some(*precision)
            }
            _ => None,
        })
        .collect();
    assert_eq!(
        ps,
        vec![
            Precision::Exact,
            Precision::Exact,
            Precision::Month,
            Precision::Decade
        ]
    );
    clean(&w);
    drop(w);
    assert!(
        doctor_log(&c.path, &DoctorOptions::default())
            .unwrap()
            .is_clean()
    );
}

#[test]
fn doctor_log_reports_a_bad_precision_byte_as_a_damaged_log_not_a_clean_one() {
    let c = crafted();
    let file = c.with(&[c.fact(9, false, 6, "Gregor")]);
    std::fs::write(&c.path, &file).unwrap();
    match doctor_log(&c.path, &DoctorOptions::default()) {
        Ok(r) => assert!(!r.is_clean(), "a bad precision byte must be a finding"),
        Err(Error::Corrupt(_)) => {}
        Err(e) => panic!("{e:?}"),
    }
}

// ================================================================ properties

#[derive(Clone, Debug)]
enum Step {
    Write {
        name: u8,
        tick: i8,
        retract: bool,
        p: u8,
    },
    Replace {
        nth: u8,
        tick: Option<i8>,
        op: Option<bool>,
        p: Option<u8>,
    },
}

fn write_step() -> impl Strategy<Value = Step> {
    (0u8..3, -4i8..5, any::<bool>(), 0u8..6).prop_map(|(name, tick, retract, p)| Step::Write {
        name,
        tick,
        retract,
        p,
    })
}

fn step() -> impl Strategy<Value = Step> {
    prop_oneof![
        3 => write_step(),
        1 => (
            any::<u8>(),
            proptest::option::of(-4i8..5),
            proptest::option::of(any::<bool>()),
            proptest::option::of(0u8..6)
        )
            .prop_map(|(nth, tick, op, p)| Step::Replace { nth, tick, op, p }),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 128,
        rng_algorithm: RngAlgorithm::ChaCha,
        rng_seed: RngSeed::Fixed(0x5052_4543),
        ..ProptestConfig::default()
    })]

    /// The same random writes run twice, once with random precisions and once with every
    /// precision Exact: identical results, errors, entry ids, indexes and answers; a
    /// history row carries the precision of the write that stored it; doctor is clean.
    #[test]
    fn precision_never_changes_what_is_stored_or_answered_only_what_is_carried(
        steps in proptest::collection::vec(write_step(), 1..30)
    ) {
        let mut a = World::in_memory(schema()).unwrap();
        let mut b = World::in_memory(schema()).unwrap();
        let ga = a.create_entity("CHARACTER", Some("greg")).unwrap();
        let gb = b.create_entity("CHARACTER", Some("greg")).unwrap();
        prop_assert_eq!(ga, gb);
        // naive model: (value, tick, op) -> precision of the write that stored it
        let mut model: Vec<((String, i64, Op), Precision)> = vec![];
        for st in &steps {
            let Step::Write { name, tick, retract, p } = st else { unreachable!() };
            let n = format!("n{name}");
            let v = s(&n);
            let tk = t(*tick as i64);
            let pa = ALL[*p as usize];
            let (ra, rb) = if *retract {
                (a.retract_with_precision(ga, "name", v.clone(), tk, pa),
                 b.retract_with_precision(gb, "name", v, tk, Precision::Exact))
            } else {
                (a.assert_with_precision(ga, "name", v.clone(), tk, pa),
                 b.assert_with_precision(gb, "name", v, tk, Precision::Exact))
            };
            match (ra, rb) {
                (Ok(ra), Ok(rb)) => {
                    prop_assert_eq!(ra.changed, rb.changed);
                    prop_assert_eq!(ra.tick, rb.tick);
                    prop_assert_eq!(&ra.entries, &rb.entries);
                    if ra.changed == 1 {
                        let op = if *retract { Op::Retract } else { Op::Assert };
                        model.push(((n, *tick as i64, op), pa));
                    }
                }
                (Err(x), Err(y)) => prop_assert_eq!(x, y),
                (x, y) => prop_assert!(false, "diverged: {:?} vs {:?}", x, y),
            }
        }
        prop_assert_eq!(a.index_dump(), b.index_dump());
        prop_assert_eq!(a.latest_tx(), b.latest_tx());
        // every history row's precision is the stored write's
        let h = hist(&a, ga, "name");
        prop_assert_eq!(h.len(), model.len());
        for (tick, op, v, p) in &h {
            let Value::Str(n) = v else { unreachable!() };
            let hit = model.iter().find(|((mn, mt, mo), _)| mn == n && mt == tick && mo == op);
            prop_assert_eq!(hit.map(|x| x.1), Some(*p), "{:?}", (tick, op, v));
        }
        for at in -5i64..6 {
            let tk = Tick(at);
            prop_assert_eq!(a.as_of(ga, "name", tk).unwrap(), b.as_of(gb, "name", tk).unwrap());
            let d = a.as_of_detail(ga, "name", tk).unwrap();
            prop_assert_eq!(
                d.iter().map(|c| c.value.clone()).collect::<Vec<_>>(),
                a.as_of(ga, "name", tk).unwrap()
            );
            // each detail row is the latest op <= at of its value, and carries its precision
            for c in &d {
                let Value::Str(n) = &c.value else { unreachable!() };
                let latest = model
                    .iter()
                    .filter(|((mn, mt, _), _)| mn == n && *mt <= at)
                    .max_by_key(|((_, mt, _), _)| *mt)
                    .unwrap();
                prop_assert_eq!(latest.0.2, Op::Assert);
                prop_assert_eq!((c.tick.0, c.precision), (latest.0.1, latest.1));
            }
        }
        clean(&a);
    }

    /// Writes and arbitrary corrections (some precision-only) on a file world: a reopen
    /// (a rebuild from the log) answers exactly the same, precision included, and doctor
    /// is clean throughout.
    #[test]
    fn a_rebuild_from_the_log_keeps_every_precision_through_corrections(
        steps in proptest::collection::vec(step(), 1..24)
    ) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("w.kala");
        let mut w = World::create(&path, schema()).unwrap();
        let g = w.create_entity("CHARACTER", Some("greg")).unwrap();
        let mut entries: Vec<EntryId> = vec![];
        let mut canons = vec![w.latest_tx()];
        for st in &steps {
            match st {
                Step::Write { name, tick, retract, p } => {
                    let v = s(&format!("n{name}"));
                    let tk = t(*tick as i64);
                    let pa = ALL[*p as usize];
                    let r = if *retract {
                        w.retract_with_precision(g, "name", v, tk, pa)
                    } else {
                        w.assert_with_precision(g, "name", v, tk, pa)
                    };
                    if let Ok(r) = r {
                        entries.extend(r.entries.iter().copied());
                    }
                }
                Step::Replace { nth, tick, op, p } => {
                    if entries.is_empty() { continue; }
                    let id = entries[*nth as usize % entries.len()];
                    let c = rep(
                        None,
                        tick.map(|x| Tick(x as i64)),
                        op.map(|r| if r { Op::Retract } else { Op::Assert }),
                        p.map(|x| ALL[x as usize]),
                    );
                    if let Ok(r) = w.correct(id, c) {
                        // a Revised is itself an entry a later correction may not point at;
                        // only base entries are kept in `entries`
                        let _ = r;
                    }
                }
            }
            canons.push(w.latest_tx());
        }
        clean(&w);
        let snapshot = |w: &World| {
            let mut out = vec![];
            for &c in &canons {
                let v = w.at_canon(c);
                out.push(format!("{:?}", v.history(g, "name").unwrap()));
                for at in -5i64..6 {
                    out.push(format!("{:?}", v.as_of_detail(g, "name", Tick(at)).unwrap()));
                }
            }
            out
        };
        let live = snapshot(&w);
        let dump = w.index_dump();
        drop(w);
        let w2 = World::open(&path).unwrap();
        prop_assert_eq!(&w2.recovery().cause, &None);
        prop_assert_eq!(snapshot(&w2), live);
        prop_assert_eq!(w2.index_dump(), dump);
        clean(&w2);
        prop_assert!(doctor_log(&path, &DoctorOptions::default()).unwrap().is_clean());
    }
}
