//! Slice 4, item 1: the V index, exact (docs/slice-4-decisions.md "Item 1";
//! CLAUDE.md "Indexes", "Assert / retract, latest wins"). RED until the
//! implementation lands: this file fails to COMPILE.
//!
//! API expected (all re-exported at the crate root):
//!
//! ```text
//! World::lookup(&self, attr: &str, value: &Value, t: Tick) -> Result<Vec<EntityId>, Error>
//! View::lookup (same signature, at the pinned canon)
//! Error::NotIndexed(String)            // the attribute name
//! IndexName::V, IndexName::VLatest
//! IndexDump { .., pub v: Vec<(AttrId, Value, EntityId, Tick, TxId, Mark)> }  // key order
//! ```
//!
//! Contract: `lookup(attr, v, t)` = every entity (or edge, for edge
//! properties) `e` for which `(e, attr, v)` holds at `t` (latest op with tick
//! <= t is an assert, DELETED keys skipped), ascending by EntityId, no
//! duplicates. On a ref attribute the answer comes from R: the sources of
//! `(s, attr, x)` for `Ref(x)` (bidirectional: the partners of x on either
//! side). Unknown attribute or inverse name -> UnknownAttribute; unindexed ->
//! NotIndexed; wrong type -> WrongValueType; NaN -> Value(NaN); an indexed
//! string over 256 bytes and a Ref to an undefined entity -> Ok(empty).

use kala_core::*;
use proptest::prelude::*;
use proptest::test_runner::{RngAlgorithm, RngSeed};

// ---------------------------------------------------------------- helpers

fn t(x: i64) -> Option<Tick> {
    Some(Tick(x))
}

fn s(x: &str) -> Value {
    Value::Str(x.into())
}

fn r(e: EntityId) -> Value {
    Value::Ref(e)
}

fn schema() -> Schema {
    Schema::new()
        .node_type("CHARACTER")
        .node_type("PLACE")
        .node_type("ITEM")
        .attribute(AttrDef::new(
            "name",
            ValueType::Str,
            Kind::State,
            Cardinality::One,
        ))
        .attribute(AttrDef::new(
            "tag",
            ValueType::Str,
            Kind::State,
            Cardinality::Many,
        ))
        .attribute(AttrDef::new(
            "rank",
            ValueType::I64,
            Kind::State,
            Cardinality::One,
        ))
        .attribute(AttrDef::new(
            "weight",
            ValueType::F64,
            Kind::State,
            Cardinality::One,
        ))
        .attribute(AttrDef::new(
            "alive",
            ValueType::Bool,
            Kind::State,
            Cardinality::One,
        ))
        .attribute(AttrDef::new(
            "title",
            ValueType::Str,
            Kind::Event,
            Cardinality::Many,
        ))
        .attribute(AttrDef::new("bio", ValueType::Str, Kind::State, Cardinality::One).unindexed())
        .attribute(AttrDef::new(
            "owns",
            ValueType::Ref,
            Kind::State,
            Cardinality::Many,
        ))
        .attribute(
            AttrDef::new("ally_of", ValueType::Ref, Kind::State, Cardinality::Many).bidirectional(),
        )
        .attribute(
            AttrDef::new("parent_of", ValueType::Ref, Kind::State, Cardinality::Many)
                .inverse("child_of"),
        )
        .attribute(AttrDef::new(
            "present",
            ValueType::Ref,
            Kind::State,
            Cardinality::Many,
        ))
        .attribute(AttrDef::new(
            "disguised",
            ValueType::Bool,
            Kind::State,
            Cardinality::One,
        ))
        .attribute(AttrDef::new(
            "note",
            ValueType::Str,
            Kind::State,
            Cardinality::One,
        ))
}

#[derive(Clone, Copy)]
struct Ents {
    edwin: EntityId,
    greg: EntityId,
    mara: EntityId,
    varrow: EntityId,
    sword: EntityId,
}

fn populate(w: &mut World) -> Ents {
    // ids ascend in creation order inside a type; CHARACTER < PLACE < ITEM
    let edwin = w.create_entity("CHARACTER", Some("edwin")).unwrap();
    let greg = w.create_entity("CHARACTER", Some("greg")).unwrap();
    let mara = w.create_entity("CHARACTER", Some("mara")).unwrap();
    let varrow = w.create_entity("PLACE", Some("varrow")).unwrap();
    w.create_entity("PLACE", Some("ashford")).unwrap();
    let sword = w.create_entity("ITEM", Some("sword")).unwrap();
    Ents {
        edwin,
        greg,
        mara,
        varrow,
        sword,
    }
}

fn mem() -> (World, Ents) {
    let mut w = World::in_memory(schema()).unwrap();
    let e = populate(&mut w);
    (w, e)
}

fn lk(w: &World, attr: &str, v: Value, at: i64) -> Vec<EntityId> {
    w.lookup(attr, &v, Tick(at)).unwrap()
}

fn only(r: WriteResult) -> EntryId {
    assert_eq!(r.entries.len(), 1);
    r.entries[0]
}

fn rep(value: Option<Value>, tick: Option<Tick>, op: Option<Op>) -> Correction {
    Correction::Replace {
        value,
        tick,
        op,
        precision: None,
    }
}

fn no_errors(w: &World) {
    let rep = doctor(w, &DoctorOptions::default());
    assert!(rep.errors().is_empty(), "{:#?}", rep.findings);
}

fn attr_id_of(w: &World, entry: EntryId) -> AttrId {
    match w.entry(entry).unwrap().body {
        log::EntryBody::Fact { attr, .. } | log::EntryBody::Revised { attr, .. } => attr,
        other => panic!("not a fact: {other:?}"),
    }
}

// ---------------------------------------------------------------- types

#[test]
fn the_entity_named_edwin_at_t_for_a_string_attribute() {
    let (mut w, e) = mem();
    w.assert(e.edwin, "name", s("Edwin"), t(10)).unwrap();
    w.assert(e.greg, "name", s("Greg"), t(10)).unwrap();
    assert_eq!(lk(&w, "name", s("Edwin"), 9), vec![]);
    assert_eq!(lk(&w, "name", s("Edwin"), 10), vec![e.edwin]);
    assert_eq!(lk(&w, "name", s("Edwin"), 10_000), vec![e.edwin]);
    assert_eq!(lk(&w, "name", s("Greg"), 10), vec![e.greg]);
    // exact: case and neighbours do not match
    assert_eq!(lk(&w, "name", s("edwin"), 10), vec![]);
    assert_eq!(lk(&w, "name", s("Edwi"), 10), vec![]);
    assert_eq!(lk(&w, "name", s("Edwin "), 10), vec![]);
    assert_eq!(lk(&w, "name", s(""), 10), vec![]);
    no_errors(&w);
}

#[test]
fn an_integer_attribute_including_extremes_and_negatives() {
    let (mut w, e) = mem();
    w.assert(e.edwin, "rank", Value::I64(i64::MIN), t(0))
        .unwrap();
    w.assert(e.greg, "rank", Value::I64(-1), t(0)).unwrap();
    w.assert(e.mara, "rank", Value::I64(i64::MAX), t(0))
        .unwrap();
    assert_eq!(lk(&w, "rank", Value::I64(i64::MIN), 0), vec![e.edwin]);
    assert_eq!(lk(&w, "rank", Value::I64(-1), 0), vec![e.greg]);
    assert_eq!(lk(&w, "rank", Value::I64(i64::MAX), 0), vec![e.mara]);
    assert_eq!(lk(&w, "rank", Value::I64(0), 0), vec![]);
    assert_eq!(lk(&w, "rank", Value::I64(1), 0), vec![]);
    no_errors(&w);
}

#[test]
fn a_float_attribute_including_negatives_infinities_and_zero() {
    let (mut w, e) = mem();
    w.assert(e.edwin, "weight", Value::F64(-2.5), t(0)).unwrap();
    w.assert(e.greg, "weight", Value::F64(f64::INFINITY), t(0))
        .unwrap();
    w.assert(e.mara, "weight", Value::F64(-0.0), t(0)).unwrap(); // stored as 0.0
    assert_eq!(lk(&w, "weight", Value::F64(-2.5), 0), vec![e.edwin]);
    assert_eq!(lk(&w, "weight", Value::F64(2.5), 0), vec![]);
    assert_eq!(lk(&w, "weight", Value::F64(f64::INFINITY), 0), vec![e.greg]);
    assert_eq!(lk(&w, "weight", Value::F64(f64::NEG_INFINITY), 0), vec![]);
    assert_eq!(lk(&w, "weight", Value::F64(0.0), 0), vec![e.mara]);
    no_errors(&w);
}

#[test]
fn a_bool_attribute() {
    let (mut w, e) = mem();
    w.assert(e.edwin, "alive", Value::Bool(true), t(0)).unwrap();
    w.assert(e.greg, "alive", Value::Bool(false), t(0)).unwrap();
    assert_eq!(lk(&w, "alive", Value::Bool(true), 0), vec![e.edwin]);
    assert_eq!(lk(&w, "alive", Value::Bool(false), 0), vec![e.greg]);
    // greg dies...: retract false, assert true is not the story; here: edwin dies
    w.retract(e.edwin, "alive", Value::Bool(true), t(5))
        .unwrap();
    w.assert(e.edwin, "alive", Value::Bool(false), t(5))
        .unwrap();
    assert_eq!(lk(&w, "alive", Value::Bool(true), 4), vec![e.edwin]);
    assert_eq!(lk(&w, "alive", Value::Bool(true), 5), vec![]);
    assert_eq!(
        lk(&w, "alive", Value::Bool(false), 5),
        vec![e.edwin, e.greg]
    );
    no_errors(&w);
}

#[test]
fn extreme_ticks() {
    let (mut w, e) = mem();
    w.assert(e.edwin, "name", s("Early"), t(i64::MIN)).unwrap();
    w.assert(e.greg, "name", s("Late"), t(i64::MAX)).unwrap();
    assert_eq!(lk(&w, "name", s("Early"), i64::MIN), vec![e.edwin]);
    assert_eq!(lk(&w, "name", s("Early"), i64::MAX), vec![e.edwin]);
    assert_eq!(lk(&w, "name", s("Late"), i64::MAX - 1), vec![]);
    assert_eq!(lk(&w, "name", s("Late"), i64::MAX), vec![e.greg]);
    w.retract(e.edwin, "name", s("Early"), t(i64::MAX)).unwrap();
    assert_eq!(lk(&w, "name", s("Early"), i64::MAX - 1), vec![e.edwin]);
    assert_eq!(lk(&w, "name", s("Early"), i64::MAX), vec![]);
}

// ---------------------------------------------------------------- latest wins

#[test]
fn a_retract_ends_the_value_and_a_later_assert_brings_it_back() {
    let (mut w, e) = mem();
    w.assert(e.edwin, "name", s("Edwin"), t(-300)).unwrap();
    w.retract(e.edwin, "name", s("Edwin"), t(-250)).unwrap();
    w.assert(e.edwin, "name", s("Edwin"), t(-200)).unwrap();
    assert_eq!(lk(&w, "name", s("Edwin"), -301), vec![]);
    assert_eq!(lk(&w, "name", s("Edwin"), -300), vec![e.edwin]);
    assert_eq!(lk(&w, "name", s("Edwin"), -251), vec![e.edwin]);
    assert_eq!(lk(&w, "name", s("Edwin"), -250), vec![]);
    assert_eq!(lk(&w, "name", s("Edwin"), -201), vec![]);
    assert_eq!(lk(&w, "name", s("Edwin"), -200), vec![e.edwin]);
    assert_eq!(lk(&w, "name", s("Edwin"), 9_999), vec![e.edwin]);
}

#[test]
fn latest_wins_is_by_tick_not_by_write_order() {
    let (mut w, e) = mem();
    // written out of order: the retract first, then the assert before it
    w.assert(e.edwin, "name", s("Edwin"), t(-100)).unwrap();
    w.retract(e.edwin, "name", s("Edwin"), t(-50)).unwrap();
    w.assert(e.edwin, "name", s("Edwin"), t(-400)).unwrap(); // prequel
    assert_eq!(lk(&w, "name", s("Edwin"), -400), vec![e.edwin]);
    assert_eq!(lk(&w, "name", s("Edwin"), -50), vec![]);
    assert_eq!(lk(&w, "name", s("Edwin"), -100), vec![e.edwin]);
}

#[test]
fn an_assert_never_ends_another_value_of_the_same_attribute() {
    let (mut w, e) = mem();
    w.assert(e.edwin, "name", s("Edwin"), t(0)).unwrap();
    w.assert(e.edwin, "name", s("Grey King"), t(10)).unwrap(); // overlap: a warning, not a rule
    assert_eq!(lk(&w, "name", s("Edwin"), 10), vec![e.edwin]);
    assert_eq!(lk(&w, "name", s("Grey King"), 10), vec![e.edwin]);
}

#[test]
fn a_value_asserted_twice_or_after_a_retract_appears_once() {
    let (mut w, e) = mem();
    w.assert(e.edwin, "name", s("Edwin"), t(0)).unwrap();
    w.assert(e.edwin, "name", s("Edwin"), t(5)).unwrap(); // no-op at 5
    w.retract(e.edwin, "name", s("Edwin"), t(8)).unwrap();
    w.assert(e.edwin, "name", s("Edwin"), t(9)).unwrap();
    for at in [0, 5, 7, 9, 100] {
        let got = lk(&w, "name", s("Edwin"), at);
        assert_eq!(got, vec![e.edwin], "at {at}");
    }
}

// ---------------------------------------------------------------- many entities

#[test]
fn two_entities_sharing_a_value_are_both_returned_sorted_by_id() {
    let (mut w, e) = mem();
    // written in reverse id order, one of them a different node type
    w.assert(e.sword, "name", s("Edwin"), t(0)).unwrap();
    w.assert(e.varrow, "name", s("Edwin"), t(0)).unwrap();
    w.assert(e.mara, "name", s("Edwin"), t(0)).unwrap();
    w.assert(e.edwin, "name", s("Edwin"), t(0)).unwrap();
    let got = lk(&w, "name", s("Edwin"), 0);
    assert_eq!(got, vec![e.edwin, e.mara, e.varrow, e.sword]);
    let mut sorted = got.clone();
    sorted.sort();
    sorted.dedup();
    assert_eq!(got, sorted);
    // only those holding at t
    w.retract(e.mara, "name", s("Edwin"), t(3)).unwrap();
    assert_eq!(
        lk(&w, "name", s("Edwin"), 3),
        vec![e.edwin, e.varrow, e.sword]
    );
    assert_eq!(
        lk(&w, "name", s("Edwin"), 2),
        vec![e.edwin, e.mara, e.varrow, e.sword]
    );
}

#[test]
fn a_cardinality_many_attribute_finds_the_entity_by_any_of_its_values() {
    let (mut w, e) = mem();
    w.assert(e.edwin, "tag", s("king"), t(0)).unwrap();
    w.assert(e.edwin, "tag", s("traitor"), t(0)).unwrap();
    w.assert(e.greg, "tag", s("traitor"), t(0)).unwrap();
    assert_eq!(lk(&w, "tag", s("king"), 0), vec![e.edwin]);
    assert_eq!(lk(&w, "tag", s("traitor"), 0), vec![e.edwin, e.greg]);
    w.retract(e.edwin, "tag", s("traitor"), t(4)).unwrap();
    assert_eq!(lk(&w, "tag", s("traitor"), 4), vec![e.greg]);
    assert_eq!(lk(&w, "tag", s("king"), 4), vec![e.edwin]);
}

#[test]
fn the_same_value_under_different_attributes_does_not_mix() {
    let (mut w, e) = mem();
    w.assert(e.edwin, "name", s("x"), t(0)).unwrap();
    w.assert(e.greg, "tag", s("x"), t(0)).unwrap();
    w.assert(e.mara, "note", s("x"), t(0)).unwrap();
    assert_eq!(lk(&w, "name", s("x"), 0), vec![e.edwin]);
    assert_eq!(lk(&w, "tag", s("x"), 0), vec![e.greg]);
    assert_eq!(lk(&w, "note", s("x"), 0), vec![e.mara]);
}

#[test]
fn values_that_are_prefixes_of_each_other_do_not_mix() {
    let (mut w, e) = mem();
    w.assert(e.edwin, "name", s("Ed"), t(0)).unwrap();
    w.assert(e.greg, "name", s("Edwin"), t(0)).unwrap();
    w.assert(e.mara, "name", s("Ed\u{0}"), t(0)).unwrap();
    assert_eq!(lk(&w, "name", s("Ed"), 0), vec![e.edwin]);
    assert_eq!(lk(&w, "name", s("Edwin"), 0), vec![e.greg]);
    assert_eq!(lk(&w, "name", s("Ed\u{0}"), 0), vec![e.mara]);
}

// ---------------------------------------------------------------- events

#[test]
fn an_event_value_holds_from_its_tick_on() {
    let (mut w, e) = mem();
    w.assert(e.edwin, "title", s("Duke"), t(-208)).unwrap();
    assert_eq!(lk(&w, "title", s("Duke"), -209), vec![]);
    assert_eq!(lk(&w, "title", s("Duke"), -208), vec![e.edwin]);
    assert_eq!(lk(&w, "title", s("Duke"), i64::MAX), vec![e.edwin]);
    // retracting an event stays an error and changes nothing here
    assert!(w.retract(e.edwin, "title", s("Duke"), t(0)).is_err());
    assert_eq!(lk(&w, "title", s("Duke"), 0), vec![e.edwin]);
}

// ---------------------------------------------------------------- edge properties

#[test]
fn an_edge_property_value_returns_the_edges_entity_id() {
    let (mut w, e) = mem();
    w.assert(e.edwin, "present", r(e.varrow), t(-300)).unwrap();
    w.assert(e.greg, "present", r(e.varrow), t(-300)).unwrap();
    let e1 = EntityId::from(w.edge_id(e.edwin, "present", e.varrow).unwrap());
    let e2 = EntityId::from(w.edge_id(e.greg, "present", e.varrow).unwrap());
    w.assert(e1, "disguised", Value::Bool(true), t(-300))
        .unwrap();
    w.assert(e2, "disguised", Value::Bool(true), t(-290))
        .unwrap();
    assert_eq!(lk(&w, "disguised", Value::Bool(true), -301), vec![]);
    assert_eq!(lk(&w, "disguised", Value::Bool(true), -300), vec![e1]);
    assert_eq!(lk(&w, "disguised", Value::Bool(true), -290), vec![e1, e2]);
    // dropped, then replaced (CLAUDE.md "Edges")
    w.retract(e1, "disguised", Value::Bool(true), t(-280))
        .unwrap();
    w.assert(e1, "disguised", Value::Bool(false), t(-280))
        .unwrap();
    assert_eq!(lk(&w, "disguised", Value::Bool(true), -280), vec![e2]);
    assert_eq!(lk(&w, "disguised", Value::Bool(false), -280), vec![e1]);
    assert_eq!(lk(&w, "disguised", Value::Bool(false), -281), vec![]);
    // an entity-held and an edge-held value are told apart by id
    w.assert(e.mara, "disguised", Value::Bool(true), t(0))
        .unwrap();
    assert_eq!(lk(&w, "disguised", Value::Bool(true), 0), vec![e.mara, e2]);
    no_errors(&w);
}

// ---------------------------------------------------------------- corrections

/// edwin name "Edwin" @10 (entry), tx known; returns (world, entities, entry, tx of the write).
fn named() -> (World, Ents, EntryId, TxId) {
    let (mut w, e) = mem();
    let entry = only(w.assert(e.edwin, "name", s("Edwin"), t(10)).unwrap());
    let tx = w.latest_tx();
    (w, e, entry, tx)
}

/// `at_canon` and `at_canon_main` agree on `(attr, value, tick)` at `tx`.
fn both_paths(w: &World, tx: TxId, attr: &str, v: Value, at: i64) -> Vec<EntityId> {
    let a = w.at_canon(tx).lookup(attr, &v, Tick(at)).unwrap();
    let b = w.at_canon_main(tx).lookup(attr, &v, Tick(at)).unwrap();
    assert_eq!(
        a, b,
        "at_canon vs at_canon_main at {tx:?}, {attr}={v:?}, tick {at}"
    );
    a
}

#[test]
fn correct_delete_removes_the_entity_from_the_latest_canon_but_not_from_older_ones() {
    let (mut w, e, entry, tx_write) = named();
    w.correct(entry, Correction::Delete).unwrap();
    let tx_del = w.latest_tx();
    assert_eq!(lk(&w, "name", s("Edwin"), 10), vec![]);
    assert_eq!(both_paths(&w, tx_del, "name", s("Edwin"), 10), vec![]);
    assert_eq!(
        both_paths(&w, tx_write, "name", s("Edwin"), 10),
        vec![e.edwin]
    );
    assert_eq!(
        w.at_canon(tx_write)
            .lookup("name", &s("Edwin"), Tick(10))
            .unwrap(),
        vec![e.edwin]
    );
    assert_eq!(
        both_paths(&w, TxId(tx_write.0 - 1), "name", s("Edwin"), 10),
        vec![]
    );
    no_errors(&w);
}

#[test]
fn correct_replace_value_moves_the_entity_from_the_old_value_to_the_new_one() {
    let (mut w, e, entry, tx_write) = named();
    w.correct(entry, rep(Some(s("Grey King")), None, None))
        .unwrap();
    let tx_rep = w.latest_tx();
    assert_eq!(lk(&w, "name", s("Edwin"), 10), vec![]);
    assert_eq!(lk(&w, "name", s("Grey King"), 10), vec![e.edwin]);
    assert_eq!(lk(&w, "name", s("Grey King"), 9), vec![]);
    // older canon still sees the old value, not the new one
    assert_eq!(
        both_paths(&w, tx_write, "name", s("Edwin"), 10),
        vec![e.edwin]
    );
    assert_eq!(both_paths(&w, tx_write, "name", s("Grey King"), 10), vec![]);
    // at the latest canon the two paths agree too
    assert_eq!(both_paths(&w, tx_rep, "name", s("Edwin"), 10), vec![]);
    assert_eq!(
        both_paths(&w, tx_rep, "name", s("Grey King"), 10),
        vec![e.edwin]
    );
    no_errors(&w);
}

#[test]
fn correct_replace_tick_moves_when_the_value_starts_holding() {
    let (mut w, e, entry, tx_write) = named();
    w.correct(entry, rep(None, t(-5), None)).unwrap();
    let tx_rep = w.latest_tx();
    assert_eq!(lk(&w, "name", s("Edwin"), -6), vec![]);
    assert_eq!(lk(&w, "name", s("Edwin"), -5), vec![e.edwin]);
    assert_eq!(
        both_paths(&w, tx_rep, "name", s("Edwin"), -5),
        vec![e.edwin]
    );
    assert_eq!(both_paths(&w, tx_write, "name", s("Edwin"), -5), vec![]);
    assert_eq!(
        both_paths(&w, tx_write, "name", s("Edwin"), 10),
        vec![e.edwin]
    );
    // moved later
    w.correct(entry, rep(None, t(50), None)).unwrap();
    assert_eq!(lk(&w, "name", s("Edwin"), 49), vec![]);
    assert_eq!(lk(&w, "name", s("Edwin"), 50), vec![e.edwin]);
    no_errors(&w);
}

#[test]
fn correct_replace_value_and_tick_together() {
    let (mut w, e, entry, tx_write) = named();
    w.correct(entry, rep(Some(s("Grey King")), t(-5), None))
        .unwrap();
    assert_eq!(lk(&w, "name", s("Edwin"), 10), vec![]);
    assert_eq!(lk(&w, "name", s("Grey King"), -6), vec![]);
    assert_eq!(lk(&w, "name", s("Grey King"), -5), vec![e.edwin]);
    assert_eq!(
        both_paths(&w, tx_write, "name", s("Edwin"), 10),
        vec![e.edwin]
    );
    assert_eq!(both_paths(&w, tx_write, "name", s("Grey King"), -5), vec![]);
}

#[test]
fn correct_replace_op_flips_an_assert_to_a_retract() {
    let (mut w, e) = mem();
    w.assert(e.edwin, "name", s("Edwin"), t(0)).unwrap();
    let ret = only(w.retract(e.edwin, "name", s("Edwin"), t(10)).unwrap());
    let tx_before = w.latest_tx();
    assert_eq!(lk(&w, "name", s("Edwin"), 10), vec![]);
    w.correct(ret, rep(None, None, Some(Op::Assert))).unwrap(); // a no-op assert? stays held
    assert_eq!(lk(&w, "name", s("Edwin"), 10), vec![e.edwin]);
    assert_eq!(both_paths(&w, tx_before, "name", s("Edwin"), 10), vec![]);
    w.correct(ret, Correction::Delete).unwrap();
    assert_eq!(lk(&w, "name", s("Edwin"), 10), vec![e.edwin]);
    no_errors(&w);
}

#[test]
fn deleting_a_retract_makes_the_value_hold_again_on_the_latest_canon_only() {
    let (mut w, e) = mem();
    w.assert(e.edwin, "name", s("Edwin"), t(0)).unwrap();
    let ret = only(w.retract(e.edwin, "name", s("Edwin"), t(10)).unwrap());
    let tx_with = w.latest_tx();
    w.correct(ret, Correction::Delete).unwrap();
    assert_eq!(lk(&w, "name", s("Edwin"), 20), vec![e.edwin]);
    assert_eq!(both_paths(&w, tx_with, "name", s("Edwin"), 20), vec![]);
    assert_eq!(
        both_paths(&w, w.latest_tx(), "name", s("Edwin"), 20),
        vec![e.edwin]
    );
}

#[test]
fn a_correction_inside_a_multi_op_txn_is_visible_after_commit() {
    let (mut w, e, entry, tx_write) = named();
    let mut tx = w.begin();
    tx.correct(entry, rep(Some(s("Grey King")), None, None))
        .unwrap();
    tx.assert(e.greg, "name", s("Edwin"), t(10)).unwrap();
    let c = tx.commit().unwrap();
    assert_eq!(lk(&w, "name", s("Edwin"), 10), vec![e.greg]);
    assert_eq!(lk(&w, "name", s("Grey King"), 10), vec![e.edwin]);
    assert_eq!(
        both_paths(&w, tx_write, "name", s("Edwin"), 10),
        vec![e.edwin]
    );
    assert_eq!(both_paths(&w, c.tx, "name", s("Edwin"), 10), vec![e.greg]);
}

#[test]
fn a_dropped_or_poisoned_txn_leaves_no_trace_in_lookup() {
    let (mut w, e) = mem();
    {
        let mut tx = w.begin();
        tx.assert(e.edwin, "name", s("Ghost"), t(0)).unwrap();
        // dropped without commit
    }
    {
        let mut tx = w.begin();
        tx.assert(e.edwin, "name", s("Ghost2"), t(0)).unwrap();
        assert!(tx.assert(e.edwin, "nope", s("x"), t(0)).is_err());
        assert!(tx.commit().is_err());
    }
    assert_eq!(lk(&w, "name", s("Ghost"), 0), vec![]);
    assert_eq!(lk(&w, "name", s("Ghost2"), 0), vec![]);
    assert_eq!(w.index_dump().v.len(), 0);
}

#[test]
fn older_canons_see_each_historic_answer_and_both_paths_agree_at_every_canon() {
    let (mut w, e) = mem();
    let base = w.latest_tx();
    let a = only(w.assert(e.edwin, "name", s("A"), t(0)).unwrap());
    w.assert(e.greg, "name", s("A"), t(5)).unwrap();
    w.retract(e.edwin, "name", s("A"), t(3)).unwrap();
    w.correct(a, rep(Some(s("B")), t(1), None)).unwrap();
    w.correct(a, Correction::Delete).unwrap();
    let mut seen = Vec::new();
    for tx in base.0..=w.latest_tx().0 {
        for v in ["A", "B"] {
            for at in -1..=6 {
                let got = both_paths(&w, TxId(tx), "name", s(v), at);
                // the pinned view equals the same question asked through as_of
                let want: Vec<EntityId> = [e.edwin, e.greg, e.mara]
                    .into_iter()
                    .filter(|&x| {
                        w.at_canon(TxId(tx))
                            .as_of(x, "name", Tick(at))
                            .unwrap()
                            .contains(&s(v))
                    })
                    .collect();
                assert_eq!(got, want, "tx {tx} {v} @{at}");
                seen.push(got);
            }
        }
    }
    assert!(seen.iter().any(|g| !g.is_empty()));
}

// ---------------------------------------------------------------- ref attributes (answered from R)

#[test]
fn a_directed_ref_lookup_returns_the_sources() {
    let (mut w, e) = mem();
    w.assert(e.greg, "owns", r(e.sword), t(0)).unwrap();
    w.assert(e.edwin, "owns", r(e.sword), t(5)).unwrap();
    assert_eq!(lk(&w, "owns", r(e.sword), -1), vec![]);
    assert_eq!(lk(&w, "owns", r(e.sword), 0), vec![e.greg]);
    assert_eq!(lk(&w, "owns", r(e.sword), 5), vec![e.edwin, e.greg]);
    w.retract(e.greg, "owns", r(e.sword), t(9)).unwrap();
    assert_eq!(lk(&w, "owns", r(e.sword), 9), vec![e.edwin]);
    // the other direction of a directed edge is not a match
    assert_eq!(lk(&w, "owns", r(e.greg), 5), vec![]);
    // an inverse-named attribute is queried by its real name
    w.assert(e.edwin, "parent_of", r(e.mara), t(0)).unwrap();
    assert_eq!(lk(&w, "parent_of", r(e.mara), 0), vec![e.edwin]);
    no_errors(&w);
}

#[test]
fn a_bidirectional_ref_lookup_returns_the_partners_whichever_side_they_were_written_on() {
    let (mut w, e) = mem();
    w.assert(e.edwin, "ally_of", r(e.mara), t(0)).unwrap();
    w.assert(e.mara, "ally_of", r(e.greg), t(0)).unwrap();
    assert_eq!(lk(&w, "ally_of", r(e.mara), 0), vec![e.edwin, e.greg]);
    assert_eq!(lk(&w, "ally_of", r(e.edwin), 0), vec![e.mara]);
    assert_eq!(lk(&w, "ally_of", r(e.greg), 0), vec![e.mara]);
    assert_eq!(lk(&w, "ally_of", r(e.mara), -1), vec![]);
    w.retract(e.mara, "ally_of", r(e.edwin), t(4)).unwrap(); // the other spelling ends it
    assert_eq!(lk(&w, "ally_of", r(e.mara), 4), vec![e.greg]);
    assert_eq!(lk(&w, "ally_of", r(e.edwin), 4), vec![]);
    // a self pair finds the entity once
    w.assert(e.edwin, "ally_of", r(e.edwin), t(0)).unwrap();
    assert_eq!(lk(&w, "ally_of", r(e.edwin), 0), vec![e.edwin, e.mara]);
    assert_eq!(lk(&w, "ally_of", r(e.edwin), 4), vec![e.edwin]);
}

#[test]
fn a_ref_lookup_follows_corrections_like_the_r_index() {
    let (mut w, e) = mem();
    let f = only(w.assert(e.greg, "owns", r(e.sword), t(0)).unwrap());
    let tx0 = w.latest_tx();
    w.correct(f, rep(Some(r(e.varrow)), None, None)).unwrap();
    assert_eq!(lk(&w, "owns", r(e.sword), 0), vec![]);
    assert_eq!(lk(&w, "owns", r(e.varrow), 0), vec![e.greg]);
    assert_eq!(both_paths(&w, tx0, "owns", r(e.sword), 0), vec![e.greg]);
}

// ---------------------------------------------------------------- errors

#[test]
fn an_unknown_attribute_and_an_inverse_name_are_unknown_attribute() {
    let (mut w, e) = mem();
    w.assert(e.edwin, "parent_of", r(e.mara), t(0)).unwrap();
    assert_eq!(
        w.lookup("nope", &s("x"), Tick(0)),
        Err(Error::UnknownAttribute("nope".into()))
    );
    assert_eq!(
        w.lookup("child_of", &r(e.edwin), Tick(0)),
        Err(Error::UnknownAttribute("child_of".into()))
    );
    let tx = w.latest_tx();
    assert_eq!(
        w.at_canon(tx).lookup("nope", &s("x"), Tick(0)),
        Err(Error::UnknownAttribute("nope".into()))
    );
    assert_eq!(
        w.at_canon(tx).lookup("child_of", &r(e.edwin), Tick(0)),
        Err(Error::UnknownAttribute("child_of".into()))
    );
}

#[test]
fn an_unindexed_attribute_is_not_indexed() {
    let (mut w, e) = mem();
    w.assert(e.edwin, "bio", s("a very long story"), t(0))
        .unwrap();
    assert_eq!(
        w.lookup("bio", &s("a very long story"), Tick(0)),
        Err(Error::NotIndexed("bio".into()))
    );
    let tx = w.latest_tx();
    assert_eq!(
        w.at_canon(tx).lookup("bio", &s("x"), Tick(0)),
        Err(Error::NotIndexed("bio".into()))
    );
    assert_eq!(
        w.at_canon_main(tx).lookup("bio", &s("x"), Tick(0)),
        Err(Error::NotIndexed("bio".into()))
    );
    assert!(Error::NotIndexed("bio".into()).to_string().contains("bio"));
}

#[test]
fn a_value_of_the_wrong_type_is_wrong_value_type() {
    let (w, _e) = mem();
    assert_eq!(
        w.lookup("name", &Value::I64(1), Tick(0)),
        Err(Error::WrongValueType {
            attr: "name".into(),
            expected: ValueType::Str,
            got: ValueType::I64,
        })
    );
    assert_eq!(
        w.lookup("rank", &s("1"), Tick(0)),
        Err(Error::WrongValueType {
            attr: "rank".into(),
            expected: ValueType::I64,
            got: ValueType::Str,
        })
    );
    assert_eq!(
        w.lookup("weight", &Value::I64(1), Tick(0)),
        Err(Error::WrongValueType {
            attr: "weight".into(),
            expected: ValueType::F64,
            got: ValueType::I64,
        })
    );
    assert_eq!(
        w.lookup("owns", &Value::I64(1), Tick(0)),
        Err(Error::WrongValueType {
            attr: "owns".into(),
            expected: ValueType::Ref,
            got: ValueType::I64,
        })
    );
    assert!(matches!(
        w.at_canon(w.latest_tx()).lookup("alive", &s("x"), Tick(0)),
        Err(Error::WrongValueType { .. })
    ));
}

#[test]
fn nan_is_a_value_error() {
    let (w, _e) = mem();
    assert_eq!(
        w.lookup("weight", &Value::F64(f64::NAN), Tick(0)),
        Err(Error::Value(ValueError::NaN))
    );
    assert_eq!(
        w.at_canon(w.latest_tx())
            .lookup("weight", &Value::F64(f64::NAN), Tick(0)),
        Err(Error::Value(ValueError::NaN))
    );
}

#[test]
fn an_indexed_string_over_256_bytes_finds_nothing_but_256_bytes_still_works() {
    let (mut w, e) = mem();
    let ok = "x".repeat(256);
    w.assert(e.edwin, "name", s(&ok), t(0)).unwrap();
    assert!(
        w.assert(e.edwin, "name", s(&"x".repeat(257)), t(0))
            .is_err()
    );
    assert_eq!(lk(&w, "name", s(&ok), 0), vec![e.edwin]);
    assert_eq!(w.lookup("name", &s(&"x".repeat(257)), Tick(0)), Ok(vec![]));
    assert_eq!(w.lookup("name", &s(&"é".repeat(129)), Tick(0)), Ok(vec![])); // 258 bytes
    assert_eq!(
        w.at_canon(w.latest_tx())
            .lookup("name", &s(&"x".repeat(5_000)), Tick(0)),
        Ok(vec![])
    );
}

#[test]
fn a_ref_to_an_undefined_entity_finds_nothing() {
    let (w, e) = mem();
    let ghost = EntityId::new(e.edwin.node_type(), 999).unwrap();
    assert_eq!(w.lookup("owns", &r(ghost), Tick(0)), Ok(vec![]));
    assert_eq!(w.lookup("ally_of", &r(ghost), Tick(0)), Ok(vec![]));
    assert_eq!(
        w.at_canon(w.latest_tx()).lookup("owns", &r(ghost), Tick(0)),
        Ok(vec![])
    );
}

#[test]
fn a_failed_lookup_changes_nothing() {
    let (mut w, e) = mem();
    w.assert(e.edwin, "name", s("Edwin"), t(0)).unwrap();
    let tx = w.latest_tx();
    let dump = w.index_dump();
    let _ = w.lookup("nope", &s("x"), Tick(0));
    let _ = w.lookup("bio", &s("x"), Tick(0));
    assert_eq!(w.latest_tx(), tx);
    assert_eq!(w.index_dump(), dump);
}

// ---------------------------------------------------------------- index_dump().v

#[test]
fn index_dump_v_has_rows_for_indexed_non_ref_attributes_in_key_order() {
    let (mut w, e) = mem();
    let a = only(w.assert(e.edwin, "name", s("Edwin"), t(10)).unwrap());
    let name = attr_id_of(&w, a);
    let tx_a = w.latest_tx();
    w.retract(e.edwin, "name", s("Edwin"), t(20)).unwrap();
    let tx_b = w.latest_tx();
    assert_eq!(
        w.index_dump().v,
        vec![
            (name, s("Edwin"), e.edwin, Tick(20), tx_b, Mark::Retract),
            (name, s("Edwin"), e.edwin, Tick(10), tx_a, Mark::Assert),
        ]
    );
    // a DELETED marker is mirrored too, newest tx first inside one key
    w.correct(a, Correction::Delete).unwrap();
    let tx_c = w.latest_tx();
    assert_eq!(
        w.index_dump().v,
        vec![
            (name, s("Edwin"), e.edwin, Tick(20), tx_b, Mark::Retract),
            (name, s("Edwin"), e.edwin, Tick(10), tx_c, Mark::Deleted),
            (name, s("Edwin"), e.edwin, Tick(10), tx_a, Mark::Assert),
        ]
    );
}

#[test]
fn index_dump_v_orders_by_attr_then_value_then_entity_then_newest_tick() {
    let (mut w, e) = mem();
    let n = only(w.assert(e.greg, "name", s("B"), t(1)).unwrap());
    w.assert(e.mara, "name", s("A"), t(1)).unwrap();
    w.assert(e.edwin, "name", s("B"), t(1)).unwrap();
    w.assert(e.edwin, "name", s("B"), t(7))
        .unwrap_or_else(|_| unreachable!()); // no-op, no row
    let rk = only(w.assert(e.greg, "rank", Value::I64(-5), t(1)).unwrap());
    let name = attr_id_of(&w, n);
    let rank = attr_id_of(&w, rk);
    let rows = w.index_dump().v;
    let got: Vec<_> = rows
        .iter()
        .map(|(a, v, en, tk, _, _)| (*a, v.clone(), *en, *tk))
        .collect();
    let mut want = vec![
        (name, s("A"), e.mara, Tick(1)),
        (name, s("B"), e.edwin, Tick(1)),
        (name, s("B"), e.greg, Tick(1)),
        (rank, Value::I64(-5), e.greg, Tick(1)),
    ];
    // attribute ids ascend in schema order: name < rank
    assert!(name < rank);
    want.sort_by_key(|(a, v, en, _)| (*a, v.encode().unwrap(), *en));
    assert_eq!(got, want);
}

#[test]
fn index_dump_v_has_no_rows_for_ref_or_unindexed_attributes() {
    let (mut w, e) = mem();
    let o = only(w.assert(e.greg, "owns", r(e.sword), t(0)).unwrap());
    let al = only(w.assert(e.edwin, "ally_of", r(e.mara), t(0)).unwrap());
    let b = only(w.assert(e.edwin, "bio", s("long"), t(0)).unwrap());
    w.assert(e.edwin, "present", r(e.varrow), t(0)).unwrap();
    let ed = EntityId::from(w.edge_id(e.edwin, "present", e.varrow).unwrap());
    let d = only(w.assert(ed, "disguised", Value::Bool(true), t(0)).unwrap());
    let skip = [attr_id_of(&w, o), attr_id_of(&w, al), attr_id_of(&w, b)];
    let disguised = attr_id_of(&w, d);
    let v = w.index_dump().v;
    assert!(v.iter().all(|row| !skip.contains(&row.0)), "{v:?}");
    assert_eq!(v.len(), 1);
    assert_eq!(v[0].0, disguised);
    assert_eq!(v[0].2, ed);
    // the E index still has every row
    assert!(w.index_dump().e.len() >= 5);
}

/// Every E row of an indexed non-ref attribute is mirrored exactly by a V row,
/// and V has nothing else.
fn v_mirrors_e(w: &World, indexed_non_ref: &[AttrId]) {
    let d = w.index_dump();
    let mut want: Vec<_> =
        d.e.iter()
            .filter(|(_, a, ..)| indexed_non_ref.contains(a))
            .map(|(en, a, v, tk, tx, m)| (*a, v.clone(), *en, *tk, *tx, *m))
            .collect();
    let mut got = d.v.clone();
    let k = |x: &(AttrId, Value, EntityId, Tick, TxId, Mark)| {
        (
            x.0,
            x.1.encode().unwrap(),
            x.2,
            std::cmp::Reverse(x.3),
            std::cmp::Reverse(x.4),
        )
    };
    want.sort_by_key(k);
    got.sort_by_key(k);
    assert_eq!(got, want);
    // and V is itself in key order
    let ks: Vec<_> = d.v.iter().map(k).collect();
    assert!(
        ks.windows(2).all(|p| p[0] <= p[1]),
        "V rows not in key order"
    );
}

#[test]
fn v_mirrors_every_e_entry_of_an_indexed_non_ref_attribute() {
    let (mut w, e) = mem();
    let a = only(w.assert(e.edwin, "name", s("Edwin"), t(10)).unwrap());
    let b = only(w.assert(e.edwin, "rank", Value::I64(3), t(10)).unwrap());
    w.assert(e.greg, "owns", r(e.sword), t(0)).unwrap();
    w.retract(e.edwin, "name", s("Edwin"), t(30)).unwrap();
    w.correct(a, rep(Some(s("Grey")), t(12), None)).unwrap();
    w.correct(b, Correction::Delete).unwrap();
    let ids = [attr_id_of(&w, a), attr_id_of(&w, b)];
    v_mirrors_e(&w, &ids);
    no_errors(&w);
}

// ---------------------------------------------------------------- files

#[test]
fn reopening_a_file_world_gives_the_same_lookups_and_a_clean_doctor() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("w.kala");
    let questions = |w: &World, e: &Ents| {
        let mut out = Vec::new();
        for at in [-1, 0, 5, 10, 11, 100] {
            for v in ["Edwin", "Grey King", "x"] {
                out.push(w.lookup("name", &s(v), Tick(at)).unwrap());
            }
            out.push(w.lookup("rank", &Value::I64(7), Tick(at)).unwrap());
            out.push(w.lookup("owns", &r(e.sword), Tick(at)).unwrap());
            out.push(w.lookup("ally_of", &r(e.mara), Tick(at)).unwrap());
        }
        out
    };
    let (before, tx, ents) = {
        let mut w = World::create(&path, schema()).unwrap();
        let e = populate(&mut w);
        let n = only(w.assert(e.edwin, "name", s("Edwin"), t(0)).unwrap());
        w.assert(e.greg, "name", s("x"), t(5)).unwrap();
        w.retract(e.greg, "name", s("x"), t(10)).unwrap();
        w.assert(e.mara, "rank", Value::I64(7), t(5)).unwrap();
        w.assert(e.greg, "owns", r(e.sword), t(5)).unwrap();
        w.assert(e.edwin, "ally_of", r(e.mara), t(5)).unwrap();
        w.correct(n, rep(Some(s("Grey King")), t(1), None)).unwrap();
        let q = questions(&w, &e);
        assert!(q.iter().any(|x| !x.is_empty()));
        (q, w.latest_tx(), e)
    };
    let w = World::open(&path).unwrap();
    assert_eq!(*w.recovery(), RecoveryReport::default());
    assert_eq!(w.latest_tx(), tx);
    assert_eq!(questions(&w, &ents), before);
    assert_eq!(
        w.lookup("name", &s("Grey King"), Tick(1)).unwrap(),
        vec![ents.edwin]
    );
    assert_eq!(w.lookup("name", &s("Edwin"), Tick(1)).unwrap(), vec![]);
    let rep = doctor(&w, &DoctorOptions::default());
    assert!(rep.errors().is_empty(), "{:#?}", rep.findings);
    let rep = doctor_log(&path, &DoctorOptions::default()).unwrap();
    assert!(rep.errors().is_empty(), "{:#?}", rep.findings);
}

// ---------------------------------------------------------------- doctor / tamper

fn tiny() -> World {
    let (mut w, e) = mem();
    w.assert(e.edwin, "name", s("Edwin"), t(10)).unwrap();
    w.assert(e.greg, "name", s("Greg"), t(10)).unwrap();
    w.retract(e.greg, "name", s("Greg"), t(20)).unwrap();
    w.assert(e.mara, "rank", Value::I64(2), t(1)).unwrap();
    w
}

fn mismatches(rep: &Report) -> Vec<IndexName> {
    rep.findings
        .iter()
        .filter_map(|f| match f {
            Finding::IndexMismatch { index, .. } => Some(*index),
            _ => None,
        })
        .collect()
}

#[test]
fn the_untampered_tiny_world_is_clean_and_has_v_rows() {
    let w = tiny();
    assert!(doctor(&w, &DoctorOptions::default()).is_clean());
    assert!(w.index_dump().v.len() >= 4);
}

#[test]
fn removing_a_row_from_v_or_v_latest_is_an_index_mismatch_for_that_index_only() {
    for which in [IndexName::V, IndexName::VLatest] {
        for nth in [0usize, 1, 3] {
            let mut w = tiny();
            w.tamper_index(Tamper::RemoveRow { index: which, nth });
            let rep = doctor(&w, &DoctorOptions::default());
            let m = mismatches(&rep);
            assert!(!m.is_empty(), "{which:?} #{nth}: {:#?}", rep.findings);
            assert!(m.iter().all(|i| *i == which), "{which:?} #{nth}: {m:?}");
            assert!(!rep.errors().is_empty());
        }
    }
}

#[test]
fn flipping_the_op_of_a_v_or_v_latest_row_is_an_index_mismatch_for_that_index_only() {
    for which in [IndexName::V, IndexName::VLatest] {
        for nth in [0usize, 1, 3] {
            let mut w = tiny();
            w.tamper_index(Tamper::FlipOp { index: which, nth });
            let rep = doctor(&w, &DoctorOptions::default());
            let m = mismatches(&rep);
            assert!(!m.is_empty(), "{which:?} #{nth}: {:#?}", rep.findings);
            assert!(m.iter().all(|i| *i == which), "{which:?} #{nth}: {m:?}");
        }
    }
}

#[test]
fn a_tampered_v_changes_what_lookup_says_but_a_tampered_e_does_not_touch_v() {
    let mut w = tiny();
    let before = lk(&w, "name", s("Edwin"), 10);
    assert_eq!(before.len(), 1);
    // V_latest serves latest-canon reads; removing the Edwin row (first in key order: name < rank,
    // "Edwin" < "Greg")  must change the answer, proving the read path uses V_latest
    w.tamper_index(Tamper::RemoveRow {
        index: IndexName::VLatest,
        nth: 0,
    });
    assert_eq!(lk(&w, "name", s("Edwin"), 10), vec![]);
    // the main V still answers an older canon
    let mut w2 = tiny();
    w2.tamper_index(Tamper::RemoveRow {
        index: IndexName::V,
        nth: 0,
    });
    assert_eq!(
        lk(&w2, "name", s("Edwin"), 10),
        before,
        "latest canon reads V_latest"
    );
    assert_eq!(
        w2.at_canon_main(w2.latest_tx())
            .lookup("name", &s("Edwin"), Tick(10))
            .unwrap(),
        vec![]
    );
}

// ---------------------------------------------------------------- property: lookup == the as_of model

#[derive(Clone, Debug)]
enum Step {
    Assert {
        attr: u8,
        who: u8,
        val: u8,
        tick: i64,
    },
    Retract {
        attr: u8,
        who: u8,
        val: u8,
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
        val: u8,
    },
    Flip {
        nth: u8,
    },
}

fn step() -> impl Strategy<Value = Step> {
    prop_oneof![
        5 => (0u8..2, 0u8..3, 0u8..3, -3i64..=3).prop_map(|(attr, who, val, tick)| Step::Assert { attr, who, val, tick }),
        3 => (0u8..2, 0u8..3, 0u8..3, -3i64..=3).prop_map(|(attr, who, val, tick)| Step::Retract { attr, who, val, tick }),
        1 => (0u8..40).prop_map(|nth| Step::Delete { nth }),
        2 => (0u8..40, -3i64..=3).prop_map(|(nth, tick)| Step::MoveTick { nth, tick }),
        2 => (0u8..40, 0u8..3).prop_map(|(nth, val)| Step::MoveValue { nth, val }),
        1 => (0u8..40).prop_map(|nth| Step::Flip { nth }),
    ]
}

const ATTRS: [&str; 2] = ["name", "rank"];

fn val_of(attr: u8, v: u8) -> Value {
    if attr == 0 {
        s(["a", "b", "c"][v as usize])
    } else {
        Value::I64(i64::from(v) - 1)
    }
}

/// The naive model: every entity whose `as_of` at `(canon, t)` holds `v`.
fn model(w: &World, tx: TxId, who: &[EntityId], attr: &str, v: &Value, at: i64) -> Vec<EntityId> {
    let mut out: Vec<EntityId> = who
        .iter()
        .copied()
        .filter(|&x| {
            // An entity not yet defined at this canon holds nothing.
            matches!(w.at_canon(tx).as_of(x, attr, Tick(at)), Ok(vals) if vals.contains(v))
        })
        .collect();
    out.sort();
    out
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 96,
        rng_algorithm: RngAlgorithm::ChaCha,
        rng_seed: RngSeed::Fixed(0x6c6f_6f6b),
        failure_persistence: None,
        ..ProptestConfig::default()
    })]

    /// `lookup(attr, v, t)` at every canon equals the set of entities whose
    /// `as_of(e, attr, t)` contains `v`, on both read paths, after random
    /// writes, retracts and corrections; V mirrors E; doctor finds no error.
    #[test]
    fn lookup_matches_the_as_of_model_at_every_canon(steps in proptest::collection::vec(step(), 0..22)) {
        let (mut w, e) = mem();
        let who = [e.edwin, e.greg, e.mara];
        let mut pool: Vec<(EntryId, u8)> = Vec::new();
        for st in steps {
            let pick = |pool: &Vec<(EntryId, u8)>, nth: u8| {
                if pool.is_empty() { None } else { Some(pool[nth as usize % pool.len()]) }
            };
            let (res, attr) = match st {
                Step::Assert { attr, who: x, val, tick } =>
                    (w.assert(who[x as usize], ATTRS[attr as usize], val_of(attr, val), t(tick)), attr),
                Step::Retract { attr, who: x, val, tick } =>
                    (w.retract(who[x as usize], ATTRS[attr as usize], val_of(attr, val), t(tick)), attr),
                Step::Delete { nth } => match pick(&pool, nth) {
                    Some((id, a)) => (w.correct(id, Correction::Delete), a),
                    None => continue,
                },
                Step::MoveTick { nth, tick } => match pick(&pool, nth) {
                    Some((id, a)) => (w.correct(id, rep(None, t(tick), None)), a),
                    None => continue,
                },
                Step::MoveValue { nth, val } => match pick(&pool, nth) {
                    Some((id, a)) => (w.correct(id, rep(Some(val_of(a, val)), None, None)), a),
                    None => continue,
                },
                Step::Flip { nth } => match pick(&pool, nth) {
                    Some((id, a)) => (w.correct(id, rep(None, None, Some(Op::Retract))), a),
                    None => continue,
                },
            };
            if let Ok(wr) = res {
                pool.extend(wr.entries.iter().map(|&id| (id, attr)));
            }
            // latest canon after every step
            for (ai, an) in ATTRS.iter().enumerate() {
                for v in 0..3u8 {
                    let val = val_of(ai as u8, v);
                    for at in -4..=4 {
                        let want = model(&w, w.latest_tx(), &who, an, &val, at);
                        prop_assert_eq!(w.lookup(an, &val, Tick(at)).unwrap(), want);
                    }
                }
            }
        }
        // every canon at the end, through both read paths
        for tx in 0..=w.latest_tx().0 {
            for (ai, an) in ATTRS.iter().enumerate() {
                for v in 0..3u8 {
                    let val = val_of(ai as u8, v);
                    for at in -4..=4 {
                        let want = model(&w, TxId(tx), &who, an, &val, at);
                        prop_assert_eq!(w.at_canon(TxId(tx)).lookup(an, &val, Tick(at)).unwrap(), want.clone());
                        prop_assert_eq!(w.at_canon_main(TxId(tx)).lookup(an, &val, Tick(at)).unwrap(), want);
                    }
                }
            }
        }
        // V mirrors E for the two indexed non-ref attributes
        let d = w.index_dump();
        let ids: Vec<AttrId> = d.e.iter().map(|r| r.1).collect::<std::collections::BTreeSet<_>>().into_iter().collect();
        v_mirrors_e(&w, &ids);
        prop_assert!(doctor(&w, &DoctorOptions::default()).errors().is_empty());
    }
}
