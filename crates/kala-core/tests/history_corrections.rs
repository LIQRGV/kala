//! Slice 3, item 4: `history` with corrections (docs/slice-3-decisions.md,
//! "Item 4" last bullet, "Core model"). `history` at canon N lists each key's
//! current entry at N and omits keys whose current entry is a DELETED marker;
//! the older canon still lists the old change points.

use kala_core::*;
use std::path::PathBuf;

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
        .attribute(
            AttrDef::new("ally_of", ValueType::Ref, Kind::State, Cardinality::Many).bidirectional(),
        )
}

struct Ents {
    greg: EntityId,
    edwin: EntityId,
    varrow: EntityId,
    ashford: EntityId,
}

fn populate(w: &mut World) -> Ents {
    Ents {
        greg: w.create_entity("CHARACTER", Some("greg")).unwrap(),
        edwin: w.create_entity("CHARACTER", Some("edwin")).unwrap(),
        varrow: w.create_entity("PLACE", Some("varrow")).unwrap(),
        ashford: w.create_entity("PLACE", Some("ashford")).unwrap(),
    }
}

fn mem() -> (World, Ents) {
    let mut w = World::in_memory(schema()).unwrap();
    let e = populate(&mut w);
    (w, e)
}

fn file() -> (tempfile::TempDir, PathBuf, World, Ents) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("w.kala");
    let mut w = World::create(&path, schema()).unwrap();
    let e = populate(&mut w);
    (dir, path, w, e)
}

fn t(x: i64) -> Option<Tick> {
    Some(Tick(x))
}

fn rep(value: Option<Value>, tick: Option<Tick>, op: Option<Op>) -> Correction {
    Correction::Replace {
        value,
        tick,
        op,
        precision: None,
    }
}

fn one(r: WriteResult) -> kala_core::log::EntryId {
    assert_eq!(r.entries.len(), 1, "{r:?}");
    r.entries[0]
}

fn hist(v: &View<'_>, e: EntityId, a: &str) -> Vec<(i64, Op, Value)> {
    v.history(e, a)
        .unwrap()
        .into_iter()
        .map(|c| (c.tick.0, c.op, c.value))
        .collect()
}

const A: Op = Op::Assert;
const R: Op = Op::Retract;

/// Greg's walk, then three corrections of the retract's chain. Returns the
/// canon after each stage: [walk, rev1, rev2, rev3].
fn chain(w: &mut World, e: &Ents) -> [TxId; 4] {
    let v = Value::Ref(e.varrow);
    w.assert(e.greg, "located_in", v.clone(), t(-300)).unwrap();
    let r = one(w.retract(e.greg, "located_in", v, t(-250)).unwrap());
    let c0 = w.latest_tx();
    w.correct(r, rep(None, t(-280), None)).unwrap(); // rev 1: moved
    let c1 = w.latest_tx();
    w.correct(r, rep(None, t(-270), None)).unwrap(); // rev 2: moved again
    let c2 = w.latest_tx();
    w.correct(r, rep(Some(Value::Ref(e.ashford)), None, None))
        .unwrap(); // rev 3: moved to another value
    let c3 = w.latest_tx();
    [c0, c1, c2, c3]
}

fn check_chain(w: &World, e: &Ents, c: [TxId; 4]) {
    let (v, a) = (Value::Ref(e.varrow), Value::Ref(e.ashford));
    let h = |tx: TxId| hist(&w.at_canon(tx), e.greg, "located_in");
    assert_eq!(h(c[0]), vec![(-300, A, v.clone()), (-250, R, v.clone())]);
    assert_eq!(h(c[1]), vec![(-300, A, v.clone()), (-280, R, v.clone())]);
    assert_eq!(h(c[2]), vec![(-300, A, v.clone()), (-270, R, v.clone())]);
    assert_eq!(h(c[3]), vec![(-300, A, v.clone()), (-270, R, a.clone())]);
    // the world's own latest read is the last canon
    assert_eq!(
        w.history(e.greg, "located_in")
            .unwrap()
            .into_iter()
            .map(|c| (c.tick.0, c.op, c.value))
            .collect::<Vec<_>>(),
        h(c[3])
    );
}

#[test]
fn a_chain_with_three_revisions_reads_right_at_each_canon() {
    let (mut w, e) = mem();
    let c = chain(&mut w, &e);
    check_chain(&w, &e, c);
}

#[test]
fn the_same_after_reopen() {
    let (_d, path, mut w, e) = file();
    let c = chain(&mut w, &e);
    check_chain(&w, &e, c);
    drop(w);
    let w2 = World::open(&path).unwrap();
    check_chain(&w2, &e, c);
}

#[test]
fn a_canon_before_any_fact_is_empty() {
    let (mut w, e) = mem();
    let before = w.latest_tx();
    chain(&mut w, &e);
    assert!(hist(&w.at_canon(before), e.greg, "located_in").is_empty());
}

#[test]
fn history_at_latest_shows_the_current_revision_only() {
    let (mut w, e) = mem();
    chain(&mut w, &e);
    let latest = w.history(e.greg, "located_in").unwrap();
    // 2 change points: the base assert and ONE retract (rev 3), not 4
    assert_eq!(latest.len(), 2);
    assert_eq!(
        latest.iter().filter(|c| c.op == Op::Retract).count(),
        1,
        "old revisions' retracts are not listed"
    );
}

#[test]
fn a_deleted_change_point_is_omitted_at_latest_and_listed_at_older_canon() {
    let (mut w, e) = mem();
    let v = Value::Ref(e.varrow);
    let a = one(w.assert(e.greg, "located_in", v.clone(), t(-300)).unwrap());
    let r = one(w.retract(e.greg, "located_in", v.clone(), t(-250)).unwrap());
    let a2 = one(w.assert(e.greg, "located_in", v.clone(), t(-200)).unwrap());
    let c0 = w.latest_tx();
    w.correct(r, Correction::Delete).unwrap();
    let c1 = w.latest_tx();
    w.correct(a2, Correction::Delete).unwrap();
    let c2 = w.latest_tx();
    w.correct(a, Correction::Delete).unwrap();
    let c3 = w.latest_tx();
    let h = |tx: TxId| hist(&w.at_canon(tx), e.greg, "located_in");
    assert_eq!(
        h(c0),
        vec![
            (-300, A, v.clone()),
            (-250, R, v.clone()),
            (-200, A, v.clone())
        ]
    );
    assert_eq!(h(c1), vec![(-300, A, v.clone()), (-200, A, v.clone())]);
    assert_eq!(h(c2), vec![(-300, A, v.clone())]);
    assert_eq!(h(c3), vec![]);
    assert_eq!(w.history(e.greg, "located_in").unwrap(), vec![]);
}

#[test]
fn a_delete_then_a_reassert_at_the_same_key_lists_the_new_entry_once() {
    let (mut w, e) = mem();
    let v = Value::Ref(e.varrow);
    let a = one(w.assert(e.greg, "located_in", v.clone(), t(-300)).unwrap());
    w.correct(a, Correction::Delete).unwrap();
    let deleted_canon = w.latest_tx();
    w.assert(e.greg, "located_in", v.clone(), t(-300)).unwrap();
    assert_eq!(
        hist(&w.at_canon(w.latest_tx()), e.greg, "located_in"),
        vec![(-300, A, v.clone())]
    );
    assert_eq!(
        hist(&w.at_canon(deleted_canon), e.greg, "located_in"),
        vec![]
    );
}

#[test]
fn an_op_flip_in_place_lists_one_change_point_per_canon() {
    let (mut w, e) = mem();
    let v = Value::Ref(e.varrow);
    let a = one(w.assert(e.greg, "located_in", v.clone(), t(-300)).unwrap());
    let c0 = w.latest_tx();
    w.correct(a, rep(None, None, Some(Op::Retract))).unwrap();
    let c1 = w.latest_tx();
    w.correct(a, rep(None, None, Some(Op::Assert))).unwrap();
    let c2 = w.latest_tx();
    let h = |tx: TxId| hist(&w.at_canon(tx), e.greg, "located_in");
    assert_eq!(h(c0), vec![(-300, A, v.clone())]);
    assert_eq!(h(c1), vec![(-300, R, v.clone())]);
    assert_eq!(h(c2), vec![(-300, A, v.clone())]);
}

#[test]
fn a_bidirectional_chain_reads_the_same_from_both_sides_at_each_canon() {
    let (mut w, e) = mem();
    let ally = one(w
        .assert(e.edwin, "ally_of", Value::Ref(e.greg), t(1))
        .unwrap());
    let c0 = w.latest_tx();
    w.correct(ally, rep(None, t(3), None)).unwrap();
    let c1 = w.latest_tx();
    w.correct(ally, rep(None, None, Some(Op::Retract))).unwrap();
    let c2 = w.latest_tx();
    w.correct(ally, Correction::Delete).unwrap();
    let c3 = w.latest_tx();
    let (g, ed) = (Value::Ref(e.greg), Value::Ref(e.edwin));
    for (canon, tick, op) in [
        (c0, 1, Some(A)),
        (c1, 3, Some(A)),
        (c2, 3, Some(R)),
        (c3, 0, None),
    ] {
        let from_g = hist(&w.at_canon(canon), e.greg, "ally_of");
        let from_e = hist(&w.at_canon(canon), e.edwin, "ally_of");
        match op {
            Some(op) => {
                assert_eq!(from_g, vec![(tick, op, ed.clone())], "{canon:?}");
                assert_eq!(from_e, vec![(tick, op, g.clone())], "{canon:?}");
            }
            None => {
                assert!(from_g.is_empty() && from_e.is_empty(), "{canon:?}");
            }
        }
    }
}

#[test]
fn a_move_to_the_tick_extremes_orders_the_history() {
    let (mut w, e) = mem();
    let v = Value::Ref(e.varrow);
    let a = one(w.assert(e.greg, "located_in", v.clone(), t(-300)).unwrap());
    let r = one(w.retract(e.greg, "located_in", v.clone(), t(-250)).unwrap());
    let c0 = w.latest_tx();
    w.correct(a, rep(None, t(i64::MAX), None)).unwrap();
    w.correct(r, rep(None, t(i64::MIN), None)).unwrap();
    assert_eq!(
        hist(&w.at_canon(w.latest_tx()), e.greg, "located_in"),
        vec![(i64::MIN, R, v.clone()), (i64::MAX, A, v.clone())]
    );
    assert_eq!(
        hist(&w.at_canon(c0), e.greg, "located_in"),
        vec![(-300, A, v.clone()), (-250, R, v)]
    );
}

#[test]
fn two_values_with_a_correction_on_one_keep_the_other_untouched() {
    let (mut w, e) = mem();
    let (v, a) = (Value::Ref(e.varrow), Value::Ref(e.ashford));
    let x = one(w.assert(e.greg, "located_in", v.clone(), t(10)).unwrap());
    w.assert(e.greg, "located_in", a.clone(), t(10)).unwrap();
    w.correct(x, rep(None, t(5), None)).unwrap();
    let h = hist(&w.at_canon(w.latest_tx()), e.greg, "located_in");
    assert_eq!(h.len(), 2);
    assert!(h.contains(&(5, A, v)));
    assert!(h.contains(&(10, A, a)));
}
