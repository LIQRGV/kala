//! Slice 3, item 6: the interval overlap check (docs/slice-3-decisions.md,
//! "Item 6"; CLAUDE.md "Attribute schema" and "Time in queries"). RED until
//! the implementation lands: this file fails to COMPILE because none of the
//! items below exist yet.
//!
//! API expected (module `overlap`, everything re-exported at the crate root):
//!
//! ```text
//! AttrDef.reverse_cardinality: Option<Cardinality>      // default None
//! AttrDef::reverse_one(self) -> AttrDef                  // Some(Cardinality::One)
//! Error::InvalidDirection(name)  at World creation for reverse_one on a
//!                                non-ref or a bidirectional attribute
//!
//! #[derive(Clone, Copy, Debug, PartialEq, Eq)]
//! pub struct Interval { pub from: Tick, pub until: Option<Tick> }   // [from, until), None = inf
//! #[derive(Clone, Copy, Debug, PartialEq, Eq)]
//! pub enum Side { Forward, Reverse }
//! #[derive(Clone, Debug, PartialEq)]
//! pub struct Overlap { pub entity: EntityId, pub attr: String, pub side: Side,
//!                      pub a: Value, pub b: Value, pub during: Interval }   // a < b by encoding
//!
//! pub fn value_intervals(changes: &[Change]) -> Vec<(Value, Vec<Interval>)>
//! pub fn find_overlaps(per_value: &[(Value, Vec<Interval>)]) -> Vec<(Value, Value, Interval)>
//! View::overlaps(&self, attr: &str) -> Result<Vec<Overlap>, Error>
//! World::overlaps(&self, attr: &str) -> Result<Vec<Overlap>, Error>
//! ```
//!
//! Ambiguities flagged in the report and NOT pinned here: the order of the
//! rows `find_overlaps` returns for different pairs (the tests compare them
//! sorted), and whether `value_intervals` lists a value that only ever had a
//! lone retract (the tests accept absent or empty).

mod common;

use kala_core::log::{EntryBody, read_log};
use kala_core::*;
use proptest::prelude::*;
use proptest::test_runner::{RngAlgorithm, RngSeed};
use std::collections::BTreeSet;
use std::path::PathBuf;

// ---------------------------------------------------------------- helpers

fn t(x: i64) -> Option<Tick> {
    Some(Tick(x))
}

fn iv(from: i64, until: Option<i64>) -> Interval {
    Interval {
        from: Tick(from),
        until: until.map(Tick),
    }
}

fn st(x: &str) -> Value {
    Value::Str(x.into())
}

fn ch(tick: i64, op: Op, value: Value) -> Change {
    Change {
        tick: Tick(tick),
        op,
        value,
    }
}

fn a_(tick: i64, v: &Value) -> Change {
    ch(tick, Op::Assert, v.clone())
}

fn r_(tick: i64, v: &Value) -> Change {
    ch(tick, Op::Retract, v.clone())
}

/// Sorts rows by (a, b, from) so tests don't pin the order across pairs.
fn sorted(mut v: Vec<(Value, Value, Interval)>) -> Vec<(Value, Value, Interval)> {
    v.sort_by_key(|(a, b, i)| (format!("{a:?}"), format!("{b:?}"), i.from));
    v
}

fn schema() -> Schema {
    Schema::new()
        .node_type("CHARACTER")
        .node_type("PLACE")
        .node_type("ITEM")
        .attribute(AttrDef::new(
            "located_in",
            ValueType::Ref,
            Kind::State,
            Cardinality::One,
        ))
        .attribute(AttrDef::new(
            "owns",
            ValueType::Ref,
            Kind::State,
            Cardinality::Many,
        ))
        .attribute(AttrDef::new(
            "name",
            ValueType::Str,
            Kind::State,
            Cardinality::One,
        ))
        .attribute(AttrDef::new(
            "born_in",
            ValueType::Ref,
            Kind::Event,
            Cardinality::One,
        ))
        .attribute(AttrDef::new(
            "bought",
            ValueType::Ref,
            Kind::Event,
            Cardinality::Many,
        ))
        .attribute(
            AttrDef::new("married_to", ValueType::Ref, Kind::State, Cardinality::One)
                .bidirectional(),
        )
        // a city has one ruler: ruler --rules--> city; forward many, reverse one
        .attribute(
            AttrDef::new("rules", ValueType::Ref, Kind::State, Cardinality::Many).reverse_one(),
        )
        // an edge property: cardinality one
        .attribute(AttrDef::new(
            "disguised",
            ValueType::Bool,
            Kind::State,
            Cardinality::One,
        ))
        // directed, with an inverse name, and a child has one parent
        .attribute(
            AttrDef::new("parent_of", ValueType::Ref, Kind::State, Cardinality::Many)
                .inverse("child_of")
                .reverse_one(),
        )
        // forward one AND reverse one
        .attribute(
            AttrDef::new("wields", ValueType::Ref, Kind::State, Cardinality::One).reverse_one(),
        )
}

struct Ents {
    edwin: EntityId,
    mara: EntityId,
    lisa: EntityId,
    greg: EntityId,
    varrow: EntityId,
    ashford: EntityId,
    carnath: EntityId,
    sword: EntityId,
    axe: EntityId,
}

fn populate(w: &mut World) -> Ents {
    // creation order = id order inside a type: edwin < mara < lisa < greg
    Ents {
        edwin: w.create_entity("CHARACTER", Some("edwin")).unwrap(),
        mara: w.create_entity("CHARACTER", Some("mara")).unwrap(),
        lisa: w.create_entity("CHARACTER", Some("lisa")).unwrap(),
        greg: w.create_entity("CHARACTER", Some("greg")).unwrap(),
        varrow: w.create_entity("PLACE", Some("varrow")).unwrap(),
        ashford: w.create_entity("PLACE", Some("ashford")).unwrap(),
        carnath: w.create_entity("PLACE", Some("carnath")).unwrap(),
        sword: w.create_entity("ITEM", Some("sword")).unwrap(),
        axe: w.create_entity("ITEM", Some("axe")).unwrap(),
    }
}

fn mem() -> (World, Ents) {
    let mut w = World::in_memory(schema()).unwrap();
    let e = populate(&mut w);
    (w, e)
}

fn ov(
    entity: EntityId,
    attr: &str,
    side: Side,
    a: Value,
    b: Value,
    from: i64,
    until: Option<i64>,
) -> Overlap {
    Overlap {
        entity,
        attr: attr.into(),
        side,
        a,
        b,
        during: iv(from, until),
    }
}

fn only(r: WriteResult) -> EntryId {
    assert_eq!(r.entries.len(), 1);
    r.entries[0]
}

/// A correction that moves the key appends (Deleted, Revised); returns the Revised.
fn revised(r: WriteResult) -> EntryId {
    assert_eq!(
        r.entries.len(),
        2,
        "a moving correction appends Deleted + Revised"
    );
    r.entries[1]
}

fn rep(value: Option<Value>, tick: Option<Tick>, op: Option<Op>) -> Correction {
    Correction::Replace { value, tick, op }
}

fn r(e: EntityId) -> Value {
    Value::Ref(e)
}

/// `World::overlaps` and the latest-canon view must agree; returns it.
fn overlaps(w: &World, attr: &str) -> Vec<Overlap> {
    let a = w.overlaps(attr).unwrap();
    assert_eq!(
        a,
        w.at_canon(w.latest_tx()).overlaps(attr).unwrap(),
        "World::overlaps vs View::overlaps at the latest canon"
    );
    assert_eq!(
        a,
        w.at_canon(TxId(u64::MAX)).overlaps(attr).unwrap(),
        "at_canon(MAX)"
    );
    a
}

// ---------------------------------------------------------------- value_intervals

#[test]
fn an_assert_and_a_retract_make_a_half_open_interval() {
    let v = Value::I64(7);
    let got = value_intervals(&[a_(200, &v), r_(250, &v)]);
    assert_eq!(got, vec![(v, vec![iv(200, Some(250))])]);
}

#[test]
fn an_assert_with_no_retract_is_open_ended() {
    let v = st("Ashford");
    assert_eq!(
        value_intervals(&[a_(601, &v)]),
        vec![(v, vec![iv(601, None)])]
    );
}

#[test]
fn no_changes_means_no_values() {
    assert_eq!(value_intervals(&[]), vec![]);
}

#[test]
fn a_retract_with_no_prior_assert_adds_nothing() {
    let v = st("x");
    let got = value_intervals(&[r_(5, &v)]);
    assert!(got.iter().all(|(_, i)| i.is_empty()), "{got:?}");
    // and it doesn't disturb a later assert/retract pair
    let got = value_intervals(&[r_(5, &v), a_(10, &v), r_(20, &v)]);
    assert_eq!(got, vec![(v, vec![iv(10, Some(20))])]);
    // a repeated retract adds nothing either
    let v = st("y");
    let got = value_intervals(&[a_(1, &v), r_(5, &v), r_(7, &v)]);
    assert_eq!(got, vec![(v, vec![iv(1, Some(5))])]);
}

#[test]
fn a_repeated_assert_does_not_restart_the_interval() {
    let v = st("x");
    let got = value_intervals(&[a_(1, &v), a_(5, &v), r_(10, &v)]);
    assert_eq!(got, vec![(v.clone(), vec![iv(1, Some(10))])]);
    let got = value_intervals(&[a_(1, &v), a_(5, &v)]);
    assert_eq!(got, vec![(v, vec![iv(1, None)])]);
}

#[test]
fn a_value_that_comes_back_has_ascending_disjoint_intervals() {
    let v = st("x");
    let got = value_intervals(&[a_(-300, &v), r_(-250, &v), a_(-200, &v), r_(-150, &v)]);
    assert_eq!(
        got,
        vec![(v, vec![iv(-300, Some(-250)), iv(-200, Some(-150))])]
    );
}

#[test]
fn adjacent_intervals_are_merged() {
    // retract and re-assert at the same tick (given in this order): the value
    // holds without a break from 1 to 9.
    let v = st("x");
    let got = value_intervals(&[a_(1, &v), r_(5, &v), a_(5, &v), r_(9, &v)]);
    assert_eq!(got, vec![(v, vec![iv(1, Some(9))])]);
}

#[test]
fn values_are_sorted_by_encoding_and_interleaved_changes_are_separated() {
    let (a, b) = (st("a"), st("b"));
    let got = value_intervals(&[
        a_(1, &b),
        a_(2, &a),
        r_(3, &b),
        r_(4, &a),
        a_(5, &b),
        a_(6, &a),
    ]);
    assert_eq!(
        got,
        vec![
            (a, vec![iv(2, Some(4)), iv(6, None)]),
            (b, vec![iv(1, Some(3)), iv(5, None)]),
        ]
    );
}

#[test]
fn value_intervals_handles_extreme_ticks() {
    let v = st("x");
    assert_eq!(
        value_intervals(&[a_(i64::MIN, &v), r_(i64::MAX, &v)]),
        vec![(v.clone(), vec![iv(i64::MIN, Some(i64::MAX))])]
    );
    assert_eq!(
        value_intervals(&[a_(i64::MAX, &v)]),
        vec![(v.clone(), vec![iv(i64::MAX, None)])]
    );
    assert_eq!(
        value_intervals(&[a_(i64::MIN, &v)]),
        vec![(v, vec![iv(i64::MIN, None)])]
    );
}

// ---------------------------------------------------------------- find_overlaps

#[test]
fn touching_intervals_are_not_an_overlap() {
    // retract Varrow @250 + assert Ashford @250: [200,250) and [250,inf)
    let got = find_overlaps(&[
        (st("Varrow"), vec![iv(200, Some(250))]),
        (st("Ashford"), vec![iv(250, None)]),
    ]);
    assert_eq!(got, vec![]);
    // CLAUDE.md: [-300,-250) then [-250,inf)
    let got = find_overlaps(&[
        (st("a"), vec![iv(-300, Some(-250))]),
        (st("b"), vec![iv(-250, None)]),
    ]);
    assert_eq!(got, vec![]);
}

#[test]
fn a_gap_is_not_an_overlap() {
    let got = find_overlaps(&[
        (st("a"), vec![iv(0, Some(10))]),
        (st("b"), vec![iv(20, None)]),
    ]);
    assert_eq!(got, vec![]);
}

#[test]
fn zero_or_one_value_never_overlaps() {
    assert_eq!(find_overlaps(&[]), vec![]);
    assert_eq!(find_overlaps(&[(st("a"), vec![iv(0, None)])]), vec![]);
    assert_eq!(
        find_overlaps(&[(st("a"), vec![iv(0, Some(5)), iv(10, None)])]),
        vec![]
    );
}

#[test]
fn a_nested_interval_overlaps_for_its_whole_length() {
    let got = find_overlaps(&[
        (st("a"), vec![iv(0, Some(100))]),
        (st("b"), vec![iv(10, Some(20))]),
    ]);
    assert_eq!(got, vec![(st("a"), st("b"), iv(10, Some(20)))]);
}

#[test]
fn a_partial_overlap_is_the_intersection() {
    let got = find_overlaps(&[
        (st("a"), vec![iv(0, Some(10))]),
        (st("b"), vec![iv(5, Some(15))]),
    ]);
    assert_eq!(got, vec![(st("a"), st("b"), iv(5, Some(10)))]);
}

#[test]
fn open_ended_intervals_overlap_to_infinity() {
    let got = find_overlaps(&[(st("a"), vec![iv(0, None)]), (st("b"), vec![iv(601, None)])]);
    assert_eq!(got, vec![(st("a"), st("b"), iv(601, None))]);
}

#[test]
fn edwin_in_varrow_and_ashford_has_two_disjoint_overlaps() {
    // CLAUDE.md "Time in queries": Varrow {[-300,-250), [-200,-150)},
    // Ashford {[-280,-180)} -> [-280,-250) and [-200,-180).
    let got = find_overlaps(&[
        (st("Ashford"), vec![iv(-280, Some(-180))]),
        (
            st("Varrow"),
            vec![iv(-300, Some(-250)), iv(-200, Some(-150))],
        ),
    ]);
    assert_eq!(
        sorted(got),
        vec![
            (st("Ashford"), st("Varrow"), iv(-280, Some(-250))),
            (st("Ashford"), st("Varrow"), iv(-200, Some(-180))),
        ]
    );
}

#[test]
fn three_values_are_checked_pairwise() {
    let got = find_overlaps(&[
        (st("a"), vec![iv(0, Some(10))]),
        (st("b"), vec![iv(5, Some(15))]),
        (st("c"), vec![iv(8, Some(20))]),
    ]);
    assert_eq!(
        sorted(got),
        vec![
            (st("a"), st("b"), iv(5, Some(10))),
            (st("a"), st("c"), iv(8, Some(10))),
            (st("b"), st("c"), iv(8, Some(15))),
        ]
    );
}

#[test]
fn the_pair_is_reported_in_encoding_order_whatever_the_input_order() {
    let got = find_overlaps(&[
        (st("b"), vec![iv(5, Some(15))]),
        (st("a"), vec![iv(0, Some(10))]),
    ]);
    assert_eq!(got, vec![(st("a"), st("b"), iv(5, Some(10)))]);
}

#[test]
fn find_overlaps_handles_extreme_ticks() {
    let got = find_overlaps(&[
        (st("a"), vec![iv(i64::MIN, Some(i64::MAX))]),
        (st("b"), vec![iv(i64::MAX - 1, None)]),
    ]);
    assert_eq!(
        got,
        vec![(st("a"), st("b"), iv(i64::MAX - 1, Some(i64::MAX)))]
    );
    // [MIN, MAX) touches [MAX, inf)
    let got = find_overlaps(&[
        (st("a"), vec![iv(i64::MIN, Some(i64::MAX))]),
        (st("b"), vec![iv(i64::MAX, None)]),
    ]);
    assert_eq!(got, vec![]);
    // both unbounded on both... [MIN, inf) vs [MIN, inf)
    let got = find_overlaps(&[
        (st("a"), vec![iv(i64::MIN, None)]),
        (st("b"), vec![iv(i64::MIN, None)]),
    ]);
    assert_eq!(got, vec![(st("a"), st("b"), iv(i64::MIN, None))]);
}

#[test]
fn the_module_path_is_kala_core_overlap() {
    let got = kala_core::overlap::find_overlaps(&[
        (st("a"), vec![iv(0, None)]),
        (st("b"), vec![iv(1, None)]),
    ]);
    assert_eq!(got.len(), 1);
    let _: kala_core::overlap::Interval = iv(0, None);
    let _: kala_core::overlap::Side = Side::Forward;
}

// ---------------------------------------------------------------- World::overlaps: cardinality one

#[test]
fn greg_in_varrow_and_ashford_from_601_then_resolved_by_the_retract() {
    // CLAUDE.md tx#50 / tx#55 / tx#57
    let (mut w, e) = mem();
    w.assert(e.greg, "located_in", r(e.varrow), t(200)).unwrap();
    assert_eq!(overlaps(&w, "located_in"), vec![]);
    w.assert(e.greg, "located_in", r(e.ashford), t(601))
        .unwrap();
    assert_eq!(
        overlaps(&w, "located_in"),
        vec![ov(
            e.greg,
            "located_in",
            Side::Forward,
            r(e.varrow),
            r(e.ashford),
            601,
            None
        )]
    );
    w.retract(e.greg, "located_in", r(e.varrow), t(250))
        .unwrap();
    assert_eq!(overlaps(&w, "located_in"), vec![]);
}

#[test]
fn moving_with_touching_ticks_is_not_flagged() {
    let (mut w, e) = mem();
    w.assert(e.greg, "located_in", r(e.varrow), t(-300))
        .unwrap();
    w.retract(e.greg, "located_in", r(e.varrow), t(-250))
        .unwrap();
    w.assert(e.greg, "located_in", r(e.ashford), t(-250))
        .unwrap();
    assert_eq!(overlaps(&w, "located_in"), vec![]);
}

#[test]
fn a_gap_between_places_is_not_reported() {
    let (mut w, e) = mem();
    w.assert(e.greg, "located_in", r(e.varrow), t(200)).unwrap();
    w.retract(e.greg, "located_in", r(e.varrow), t(250))
        .unwrap();
    w.assert(e.greg, "located_in", r(e.ashford), t(601))
        .unwrap();
    assert_eq!(overlaps(&w, "located_in"), vec![]);
}

#[test]
fn a_prequel_write_can_create_an_overlap_earlier_in_the_timeline() {
    let (mut w, e) = mem();
    w.assert(e.greg, "located_in", r(e.varrow), t(200)).unwrap();
    w.retract(e.greg, "located_in", r(e.varrow), t(250))
        .unwrap();
    w.assert(e.greg, "located_in", r(e.ashford), t(601))
        .unwrap();
    assert_eq!(overlaps(&w, "located_in"), vec![]);
    // written last, happened in the middle: Ashford now holds from 220 on
    // (nothing retracts it), so Varrow [200,250) and Ashford [220,inf) overlap
    w.assert(e.greg, "located_in", r(e.ashford), t(220))
        .unwrap();
    assert_eq!(
        overlaps(&w, "located_in"),
        vec![ov(
            e.greg,
            "located_in",
            Side::Forward,
            r(e.varrow),
            r(e.ashford),
            220,
            Some(250)
        )]
    );
}

#[test]
fn a_prequel_before_the_first_fact_overlaps_the_whole_first_stay() {
    let (mut w, e) = mem();
    w.assert(e.greg, "located_in", r(e.varrow), t(200)).unwrap();
    w.retract(e.greg, "located_in", r(e.varrow), t(250))
        .unwrap();
    w.assert(e.greg, "located_in", r(e.ashford), t(-50))
        .unwrap();
    assert_eq!(
        overlaps(&w, "located_in"),
        vec![ov(
            e.greg,
            "located_in",
            Side::Forward,
            r(e.varrow),
            r(e.ashford),
            200,
            Some(250)
        )]
    );
}

#[test]
fn only_the_entity_with_the_overlap_is_reported() {
    let (mut w, e) = mem();
    w.assert(e.edwin, "located_in", r(e.varrow), t(0)).unwrap();
    w.assert(e.mara, "located_in", r(e.varrow), t(0)).unwrap(); // many in one place: fine
    w.assert(e.greg, "located_in", r(e.varrow), t(0)).unwrap();
    w.assert(e.greg, "located_in", r(e.carnath), t(10)).unwrap();
    let got = overlaps(&w, "located_in");
    assert_eq!(
        got,
        vec![ov(
            e.greg,
            "located_in",
            Side::Forward,
            r(e.varrow),
            r(e.carnath),
            10,
            None
        )]
    );
}

#[test]
fn a_non_ref_cardinality_one_attribute_overlaps_too() {
    let (mut w, e) = mem();
    w.assert(e.greg, "name", st("Greg"), t(0)).unwrap();
    assert_eq!(overlaps(&w, "name"), vec![]);
    w.assert(e.greg, "name", st("Gregor"), t(10)).unwrap();
    assert_eq!(
        overlaps(&w, "name"),
        vec![ov(
            e.greg,
            "name",
            Side::Forward,
            st("Greg"),
            st("Gregor"),
            10,
            None
        )]
    );
    // the author ends the old name at 50: [10,50) remains
    w.retract(e.greg, "name", st("Greg"), t(50)).unwrap();
    assert_eq!(
        overlaps(&w, "name"),
        vec![ov(
            e.greg,
            "name",
            Side::Forward,
            st("Greg"),
            st("Gregor"),
            10,
            Some(50)
        )]
    );
}

#[test]
fn an_overlap_that_touches_is_resolved_by_moving_the_retract_to_the_start() {
    let (mut w, e) = mem();
    w.assert(e.greg, "name", st("Greg"), t(0)).unwrap();
    let rt = only(w.retract(e.greg, "name", st("Greg"), t(50)).unwrap());
    w.assert(e.greg, "name", st("Gregor"), t(10)).unwrap();
    assert_eq!(overlaps(&w, "name").len(), 1);
    // "he actually dropped the old name at 10": touching, so clean
    w.correct(rt, rep(None, t(10), None)).unwrap();
    assert_eq!(overlaps(&w, "name"), vec![]);
}

#[test]
fn extreme_ticks_in_a_world() {
    let (mut w, e) = mem();
    w.assert(e.greg, "located_in", r(e.varrow), t(i64::MIN))
        .unwrap();
    w.retract(e.greg, "located_in", r(e.varrow), t(i64::MAX))
        .unwrap();
    w.assert(e.greg, "located_in", r(e.ashford), t(i64::MAX))
        .unwrap();
    assert_eq!(overlaps(&w, "located_in"), vec![], "touching at MAX");
    w.assert(e.greg, "located_in", r(e.carnath), t(0)).unwrap();
    assert_eq!(
        overlaps(&w, "located_in"),
        vec![
            ov(
                e.greg,
                "located_in",
                Side::Forward,
                r(e.varrow),
                r(e.carnath),
                0,
                Some(i64::MAX)
            ),
            ov(
                e.greg,
                "located_in",
                Side::Forward,
                r(e.ashford),
                r(e.carnath),
                i64::MAX,
                None
            ),
        ]
    );
}

#[test]
fn cardinality_many_event_and_unknown_attributes() {
    let (mut w, e) = mem();
    // many: overlapping values are normal
    w.assert(e.greg, "owns", r(e.sword), t(0)).unwrap();
    w.assert(e.greg, "owns", r(e.axe), t(5)).unwrap();
    assert_eq!(overlaps(&w, "owns"), vec![]);
    // events: never overlap, whatever the cardinality label
    w.assert(e.greg, "born_in", r(e.varrow), t(0)).unwrap();
    w.assert(e.greg, "born_in", r(e.ashford), t(5)).unwrap();
    assert_eq!(overlaps(&w, "born_in"), vec![]);
    w.assert(e.greg, "bought", r(e.sword), t(0)).unwrap();
    w.assert(e.greg, "bought", r(e.axe), t(0)).unwrap();
    assert_eq!(overlaps(&w, "bought"), vec![]);
    // unknown
    assert_eq!(
        w.overlaps("nope"),
        Err(Error::UnknownAttribute("nope".into()))
    );
    assert_eq!(
        w.at_canon(w.latest_tx()).overlaps("nope"),
        Err(Error::UnknownAttribute("nope".into()))
    );
}

#[test]
fn an_empty_world_has_no_overlaps() {
    let (w, _e) = mem();
    for a in [
        "located_in",
        "owns",
        "name",
        "born_in",
        "married_to",
        "rules",
        "wields",
    ] {
        assert_eq!(overlaps(&w, a), vec![], "{a}");
    }
}

// ---------------------------------------------------------------- corrections and older canon

#[test]
fn corrections_change_the_overlap_set() {
    let (mut w, e) = mem();
    w.assert(e.greg, "located_in", r(e.varrow), t(200)).unwrap();
    let rv = only(
        w.retract(e.greg, "located_in", r(e.varrow), t(250))
            .unwrap(),
    );
    let ash = only(
        w.assert(e.greg, "located_in", r(e.ashford), t(220))
            .unwrap(),
    );
    let ovl = |from, until| {
        vec![ov(
            e.greg,
            "located_in",
            Side::Forward,
            r(e.varrow),
            r(e.ashford),
            from,
            until,
        )]
    };
    assert_eq!(overlaps(&w, "located_in"), ovl(220, Some(250)));

    // move the retract earlier: [220,230)
    let rv2 = revised(w.correct(rv, rep(None, t(230), None)).unwrap());
    assert_eq!(overlaps(&w, "located_in"), ovl(220, Some(230)));

    // move it to 220: touching, clean
    let rv3 = revised(w.correct(rv2, rep(None, t(220), None)).unwrap());
    assert_eq!(overlaps(&w, "located_in"), vec![]);

    // delete the retract: Varrow holds forever, overlap [220, inf)
    w.correct(rv3, Correction::Delete).unwrap();
    assert_eq!(overlaps(&w, "located_in"), ovl(220, None));

    // move Ashford's assert to 300
    let ash2 = revised(w.correct(ash, rep(None, t(300), None)).unwrap());
    assert_eq!(overlaps(&w, "located_in"), ovl(300, None));

    // delete it: nothing left to overlap
    w.correct(ash2, Correction::Delete).unwrap();
    assert_eq!(overlaps(&w, "located_in"), vec![]);
}

#[test]
fn a_correction_that_changes_the_value_moves_the_overlap_to_the_new_value() {
    let (mut w, e) = mem();
    w.assert(e.greg, "located_in", r(e.varrow), t(0)).unwrap();
    let x = only(w.assert(e.greg, "located_in", r(e.ashford), t(10)).unwrap());
    w.correct(x, rep(Some(r(e.carnath)), None, None)).unwrap();
    assert_eq!(
        overlaps(&w, "located_in"),
        vec![ov(
            e.greg,
            "located_in",
            Side::Forward,
            r(e.varrow),
            r(e.carnath),
            10,
            None
        )]
    );
}

#[test]
fn an_older_canon_shows_the_older_overlap_set() {
    let (mut w, e) = mem();
    w.assert(e.greg, "located_in", r(e.varrow), t(200)).unwrap();
    let tx_clean = w.latest_tx();
    w.assert(e.greg, "located_in", r(e.ashford), t(601))
        .unwrap();
    let tx_overlap = w.latest_tx();
    w.retract(e.greg, "located_in", r(e.varrow), t(250))
        .unwrap();
    let tx_resolved = w.latest_tx();
    let want = vec![ov(
        e.greg,
        "located_in",
        Side::Forward,
        r(e.varrow),
        r(e.ashford),
        601,
        None,
    )];
    assert_eq!(w.at_canon(tx_clean).overlaps("located_in").unwrap(), vec![]);
    assert_eq!(w.at_canon(tx_overlap).overlaps("located_in").unwrap(), want);
    assert_eq!(
        w.at_canon(tx_resolved).overlaps("located_in").unwrap(),
        vec![]
    );
    assert_eq!(w.at_canon(TxId(0)).overlaps("located_in").unwrap(), vec![]);
    // a later correction doesn't change what the older canon says
    let x = only(
        w.assert(e.greg, "located_in", r(e.carnath), t(700))
            .unwrap(),
    );
    w.correct(x, Correction::Delete).unwrap();
    assert_eq!(w.at_canon(tx_overlap).overlaps("located_in").unwrap(), want);
    assert_eq!(overlaps(&w, "located_in"), vec![]);
}

// ---------------------------------------------------------------- reverse one

#[test]
fn two_rulers_of_one_city_overlap_on_the_reverse_side() {
    let (mut w, e) = mem();
    w.assert(e.edwin, "rules", r(e.carnath), t(10)).unwrap();
    assert_eq!(overlaps(&w, "rules"), vec![]);
    w.assert(e.mara, "rules", r(e.carnath), t(20)).unwrap();
    // forward `rules` is many: only the reverse side (the city) is reported
    assert_eq!(
        overlaps(&w, "rules"),
        vec![ov(
            e.carnath,
            "rules",
            Side::Reverse,
            r(e.edwin),
            r(e.mara),
            20,
            None
        )]
    );
    // a hand-over at 20 (retract + assert at the same tick) touches: clean
    w.retract(e.edwin, "rules", r(e.carnath), t(20)).unwrap();
    assert_eq!(overlaps(&w, "rules"), vec![]);
}

#[test]
fn a_reverse_overlap_is_judged_over_the_whole_timeline() {
    let (mut w, e) = mem();
    w.assert(e.edwin, "rules", r(e.carnath), t(10)).unwrap();
    w.retract(e.edwin, "rules", r(e.carnath), t(50)).unwrap();
    w.assert(e.mara, "rules", r(e.carnath), t(60)).unwrap();
    assert_eq!(overlaps(&w, "rules"), vec![]);
    // prequel: Mara also ruled from 30 (no retract before 60)
    w.assert(e.mara, "rules", r(e.carnath), t(30)).unwrap();
    assert_eq!(
        overlaps(&w, "rules"),
        vec![ov(
            e.carnath,
            "rules",
            Side::Reverse,
            r(e.edwin),
            r(e.mara),
            30,
            Some(50)
        )]
    );
}

#[test]
fn one_ruler_of_two_cities_is_fine_when_only_the_reverse_side_is_one() {
    let (mut w, e) = mem();
    w.assert(e.edwin, "rules", r(e.carnath), t(0)).unwrap();
    w.assert(e.edwin, "rules", r(e.varrow), t(0)).unwrap();
    assert_eq!(overlaps(&w, "rules"), vec![]);
}

#[test]
fn a_directed_attribute_without_a_reverse_label_ignores_shared_targets() {
    let (mut w, e) = mem();
    // located_in: reverse_cardinality None: many people per place
    w.assert(e.edwin, "located_in", r(e.varrow), t(0)).unwrap();
    w.assert(e.mara, "located_in", r(e.varrow), t(0)).unwrap();
    assert_eq!(overlaps(&w, "located_in"), vec![]);
}

#[test]
fn forward_and_reverse_labels_are_independent() {
    let (mut w, e) = mem();
    // wields: forward one AND reverse one.
    // forward only: Greg wields two things, each wielded by nobody else
    w.assert(e.greg, "wields", r(e.sword), t(0)).unwrap();
    w.assert(e.greg, "wields", r(e.axe), t(5)).unwrap();
    let greg_fwd = ov(
        e.greg,
        "wields",
        Side::Forward,
        r(e.sword),
        r(e.axe),
        5,
        None,
    );
    assert_eq!(overlaps(&w, "wields"), vec![greg_fwd.clone()]);
    // reverse as well: Edwin wields the sword from 3, so the sword has two
    // wielders (Edwin and Greg) from 3; Edwin himself wields one thing.
    w.assert(e.edwin, "wields", r(e.sword), t(3)).unwrap();
    let sword_rev = ov(
        e.sword,
        "wields",
        Side::Reverse,
        r(e.edwin),
        r(e.greg),
        3,
        None,
    );
    // sorted by entity: greg (CHARACTER) before sword (ITEM)
    assert_eq!(overlaps(&w, "wields"), vec![greg_fwd, sword_rev]);
}

#[test]
fn reverse_only_overlap_leaves_the_forward_side_clean() {
    let (mut w, e) = mem();
    w.assert(e.edwin, "wields", r(e.sword), t(0)).unwrap();
    w.assert(e.mara, "wields", r(e.sword), t(0)).unwrap();
    assert_eq!(
        overlaps(&w, "wields"),
        vec![ov(
            e.sword,
            "wields",
            Side::Reverse,
            r(e.edwin),
            r(e.mara),
            0,
            None
        )]
    );
}

#[test]
fn one_entity_with_both_sides_lists_forward_before_reverse() {
    let (mut w, e) = mem();
    // Greg wields sword and axe from 0 (forward overlap on Greg) and is
    // himself "wielded" by Edwin and Mara from 0 (reverse overlap on Greg).
    w.assert(e.greg, "wields", r(e.sword), t(0)).unwrap();
    w.assert(e.greg, "wields", r(e.axe), t(0)).unwrap();
    w.assert(e.edwin, "wields", r(e.greg), t(0)).unwrap();
    w.assert(e.mara, "wields", r(e.greg), t(0)).unwrap();
    assert_eq!(
        overlaps(&w, "wields"),
        vec![
            ov(
                e.greg,
                "wields",
                Side::Forward,
                r(e.sword),
                r(e.axe),
                0,
                None
            ),
            ov(
                e.greg,
                "wields",
                Side::Reverse,
                r(e.edwin),
                r(e.mara),
                0,
                None
            ),
        ]
    );
}

// ---------------------------------------------------------------- bidirectional

#[test]
fn bidirectional_one_with_the_smaller_id_as_the_shared_partner() {
    // edwin < mara < lisa. Edwin married to Mara and to Lisa: both edges are
    // stored on Edwin's E side (R on Mara and Lisa).
    let (mut w, e) = mem();
    w.assert(e.edwin, "married_to", r(e.mara), t(10)).unwrap();
    assert_eq!(overlaps(&w, "married_to"), vec![]);
    w.assert(e.edwin, "married_to", r(e.lisa), t(20)).unwrap();
    assert_eq!(
        overlaps(&w, "married_to"),
        vec![ov(
            e.edwin,
            "married_to",
            Side::Forward,
            r(e.mara),
            r(e.lisa),
            20,
            None
        )]
    );
}

#[test]
fn bidirectional_one_with_the_larger_id_as_the_shared_partner() {
    // Lisa is the larger id: both edges are stored E|edwin.. and E|mara.. with
    // R|lisa.. for both. Lisa's partners (Edwin, Mara) overlap from 20.
    let (mut w, e) = mem();
    w.assert(e.edwin, "married_to", r(e.lisa), t(10)).unwrap();
    w.assert(e.mara, "married_to", r(e.lisa), t(20)).unwrap();
    assert_eq!(
        overlaps(&w, "married_to"),
        vec![ov(
            e.lisa,
            "married_to",
            Side::Forward,
            r(e.edwin),
            r(e.mara),
            20,
            None
        )]
    );
}

#[test]
fn bidirectional_one_with_the_middle_id_as_the_shared_partner() {
    // Mara is in the middle: edwin->mara is stored on Edwin's side, mara->lisa
    // on Mara's side; Mara sees both on the merged E+R view.
    let (mut w, e) = mem();
    w.assert(e.edwin, "married_to", r(e.mara), t(10)).unwrap();
    w.assert(e.mara, "married_to", r(e.lisa), t(20)).unwrap();
    assert_eq!(
        overlaps(&w, "married_to"),
        vec![ov(
            e.mara,
            "married_to",
            Side::Forward,
            r(e.edwin),
            r(e.lisa),
            20,
            None
        )]
    );
}

#[test]
fn the_write_direction_of_a_bidirectional_pair_does_not_change_the_answer() {
    let forward = {
        let (mut w, e) = mem();
        w.assert(e.edwin, "married_to", r(e.lisa), t(10)).unwrap();
        w.assert(e.mara, "married_to", r(e.lisa), t(20)).unwrap();
        (overlaps(&w, "married_to"), e.lisa)
    };
    let (mut w, e) = mem();
    w.assert(e.lisa, "married_to", r(e.edwin), t(10)).unwrap();
    w.assert(e.lisa, "married_to", r(e.mara), t(20)).unwrap();
    let got = overlaps(&w, "married_to");
    assert_eq!(got, forward.0);
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].entity, forward.1);
    assert_eq!(got[0].side, Side::Forward);
}

#[test]
fn bidirectional_overlaps_are_reported_for_every_entity_that_has_one() {
    // Edwin: Mara and Lisa overlap; Mara: Edwin and Greg overlap.
    let (mut w, e) = mem();
    w.assert(e.edwin, "married_to", r(e.mara), t(0)).unwrap();
    w.assert(e.edwin, "married_to", r(e.lisa), t(5)).unwrap();
    w.assert(e.mara, "married_to", r(e.greg), t(7)).unwrap();
    assert_eq!(
        overlaps(&w, "married_to"),
        vec![
            ov(
                e.edwin,
                "married_to",
                Side::Forward,
                r(e.mara),
                r(e.lisa),
                5,
                None
            ),
            ov(
                e.mara,
                "married_to",
                Side::Forward,
                r(e.edwin),
                r(e.greg),
                7,
                None
            ),
        ]
    );
}

#[test]
fn a_retract_from_either_side_resolves_a_bidirectional_overlap() {
    let (mut w, e) = mem();
    w.assert(e.edwin, "married_to", r(e.mara), t(10)).unwrap();
    w.assert(e.edwin, "married_to", r(e.lisa), t(20)).unwrap();
    assert_eq!(overlaps(&w, "married_to").len(), 1);
    // written from the other end (Mara's side): same edge, same retract
    w.retract(e.mara, "married_to", r(e.edwin), t(20)).unwrap();
    assert_eq!(overlaps(&w, "married_to"), vec![]);
}

// ---------------------------------------------------------------- sorting

#[test]
fn the_result_is_sorted_by_entity_side_a_b_and_start() {
    let (mut w, e) = mem();
    // later entity written first: Greg (CHARACTER#4) before Edwin (#1) in write order
    w.assert(e.greg, "located_in", r(e.varrow), t(0)).unwrap();
    w.assert(e.greg, "located_in", r(e.ashford), t(0)).unwrap();
    // Edwin: Varrow {[0,10),[20,30)} vs Ashford {[5,25)} and Carnath [8, inf)
    w.assert(e.edwin, "located_in", r(e.varrow), t(0)).unwrap();
    w.retract(e.edwin, "located_in", r(e.varrow), t(10))
        .unwrap();
    w.assert(e.edwin, "located_in", r(e.varrow), t(20)).unwrap();
    w.retract(e.edwin, "located_in", r(e.varrow), t(30))
        .unwrap();
    w.assert(e.edwin, "located_in", r(e.ashford), t(5)).unwrap();
    w.retract(e.edwin, "located_in", r(e.ashford), t(25))
        .unwrap();
    w.assert(e.edwin, "located_in", r(e.carnath), t(8)).unwrap();
    let f = |a, b, from, until| {
        ov(
            e.edwin,
            "located_in",
            Side::Forward,
            r(a),
            r(b),
            from,
            until,
        )
    };
    assert_eq!(
        overlaps(&w, "located_in"),
        vec![
            f(e.varrow, e.ashford, 5, Some(10)),
            f(e.varrow, e.ashford, 20, Some(25)),
            f(e.varrow, e.carnath, 8, Some(10)),
            f(e.varrow, e.carnath, 20, Some(30)),
            f(e.ashford, e.carnath, 8, Some(25)),
            ov(
                e.greg,
                "located_in",
                Side::Forward,
                r(e.varrow),
                r(e.ashford),
                0,
                None
            ),
        ]
    );
}

// ---------------------------------------------------------------- schema

#[test]
fn reverse_one_is_a_builder_and_off_by_default() {
    let plain = AttrDef::new("rules", ValueType::Ref, Kind::State, Cardinality::Many);
    assert_eq!(plain.reverse_cardinality, None);
    let rev = plain.clone().reverse_one();
    assert_eq!(rev.reverse_cardinality, Some(Cardinality::One));
    assert_eq!(rev.name, "rules");
    assert_eq!(rev.cardinality, Cardinality::Many);
}

#[test]
fn reverse_one_on_a_bidirectional_attribute_is_invalid_direction() {
    let s = Schema::new().node_type("CHARACTER").attribute(
        AttrDef::new("married_to", ValueType::Ref, Kind::State, Cardinality::One)
            .bidirectional()
            .reverse_one(),
    );
    assert!(matches!(
        World::in_memory(s.clone()),
        Err(Error::InvalidDirection(n)) if n == "married_to"
    ));
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("w.kala");
    assert!(matches!(
        World::create(&path, s),
        Err(Error::InvalidDirection(n)) if n == "married_to"
    ));
}

#[test]
fn reverse_one_on_a_non_ref_attribute_is_invalid_direction() {
    for vt in [ValueType::Str, ValueType::I64, ValueType::Bool] {
        let s = Schema::new()
            .node_type("CHARACTER")
            .attribute(AttrDef::new("x", vt, Kind::State, Cardinality::One).reverse_one());
        assert!(
            matches!(World::in_memory(s), Err(Error::InvalidDirection(n)) if n == "x"),
            "{vt:?}"
        );
    }
}

#[test]
fn reverse_one_on_a_directed_ref_is_valid_with_an_inverse_name_too() {
    let s = Schema::new().node_type("CHARACTER").attribute(
        AttrDef::new("parent_of", ValueType::Ref, Kind::State, Cardinality::Many)
            .inverse("child_of")
            .reverse_one(),
    );
    assert!(World::in_memory(s).is_ok());
}

#[test]
fn reverse_one_survives_create_and_reopen_and_is_in_the_log() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("w.kala");
    let mut w = World::create(&path, schema()).unwrap();
    let e = populate(&mut w);
    w.assert(e.edwin, "rules", r(e.carnath), t(10)).unwrap();
    w.assert(e.mara, "rules", r(e.carnath), t(20)).unwrap();
    let want = vec![ov(
        e.carnath,
        "rules",
        Side::Reverse,
        r(e.edwin),
        r(e.mara),
        20,
        None,
    )];
    assert_eq!(w.overlaps("rules").unwrap(), want);
    drop(w);

    // the DefineAttr entries carry reverse_cardinality
    let mut seen = Vec::new();
    for en in read_log(&path).unwrap() {
        if let EntryBody::DefineAttr { def, .. } = en.body {
            seen.push((def.name.clone(), def.reverse_cardinality));
        }
    }
    let rc = |n: &str| seen.iter().find(|(m, _)| m == n).unwrap().1;
    assert_eq!(rc("rules"), Some(Cardinality::One));
    assert_eq!(rc("wields"), Some(Cardinality::One));
    assert_eq!(rc("located_in"), None);
    assert_eq!(rc("married_to"), None);
    assert_eq!(rc("name"), None);

    let mut w = World::open(&path).unwrap();
    assert_eq!(w.overlaps("rules").unwrap(), want, "after reopen");
    // and it keeps working for new writes on the reopened world
    w.assert(e.lisa, "rules", r(e.carnath), t(30)).unwrap();
    assert_eq!(w.overlaps("rules").unwrap().len(), 3);
    drop(w);
    let w = World::open(&path).unwrap();
    assert_eq!(w.overlaps("rules").unwrap().len(), 3, "reopened twice");
}

#[test]
fn overlaps_of_a_reopened_world_equal_the_live_ones() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("w.kala");
    let mut w = World::create(&path, schema()).unwrap();
    let e = populate(&mut w);
    w.assert(e.greg, "located_in", r(e.varrow), t(200)).unwrap();
    w.assert(e.greg, "located_in", r(e.ashford), t(601))
        .unwrap();
    let x = only(w.assert(e.greg, "name", st("Greg"), t(0)).unwrap());
    w.assert(e.greg, "name", st("Gregor"), t(1)).unwrap();
    w.correct(x, rep(None, t(5), None)).unwrap();
    w.assert(e.edwin, "married_to", r(e.mara), t(1)).unwrap();
    w.assert(e.edwin, "married_to", r(e.lisa), t(2)).unwrap();
    let live: Vec<_> = ["located_in", "name", "married_to", "rules", "wields"]
        .iter()
        .map(|a| w.overlaps(a).unwrap())
        .collect();
    assert!(live.iter().take(3).all(|v| !v.is_empty()));
    drop(w);
    let w = World::open(&path).unwrap();
    let again: Vec<_> = ["located_in", "name", "married_to", "rules", "wields"]
        .iter()
        .map(|a| w.overlaps(a).unwrap())
        .collect();
    assert_eq!(live, again);
}

// ---------------------------------------------------------------- confirm: more shapes

#[test]
fn a_self_pair_plus_another_partner_overlaps_once_on_the_entity_itself() {
    // edwin < mara. Edwin married to himself from 0 and to Mara from 5:
    // Edwin's partners {Edwin, Mara} overlap from 5; Mara has one partner.
    let (mut w, e) = mem();
    w.assert(e.edwin, "married_to", r(e.edwin), t(0)).unwrap();
    assert_eq!(overlaps(&w, "married_to"), vec![], "a lone self pair");
    w.assert(e.edwin, "married_to", r(e.mara), t(5)).unwrap();
    let want = vec![ov(
        e.edwin,
        "married_to",
        Side::Forward,
        r(e.edwin),
        r(e.mara),
        5,
        None,
    )];
    assert_eq!(overlaps(&w, "married_to"), want);
    // the same edge written from Mara's end changes nothing
    assert_eq!(
        w.assert(e.mara, "married_to", r(e.edwin), t(5))
            .unwrap()
            .changed,
        0
    );
    assert_eq!(overlaps(&w, "married_to"), want);
    // ending the self pair at 5 touches the new marriage: clean
    w.retract(e.edwin, "married_to", r(e.edwin), t(5)).unwrap();
    assert_eq!(overlaps(&w, "married_to"), vec![]);
}

#[test]
fn a_self_pair_on_the_larger_entity_lists_the_smaller_partner_first() {
    // Mara is married to herself from 0 and Edwin (smaller id) from 5.
    let (mut w, e) = mem();
    w.assert(e.mara, "married_to", r(e.mara), t(0)).unwrap();
    w.assert(e.edwin, "married_to", r(e.mara), t(5)).unwrap();
    assert_eq!(
        overlaps(&w, "married_to"),
        vec![ov(
            e.mara,
            "married_to",
            Side::Forward,
            r(e.edwin),
            r(e.mara),
            5,
            None
        )]
    );
}

#[test]
fn a_self_pair_with_two_other_partners_reports_each_pair_once() {
    let (mut w, e) = mem();
    w.assert(e.edwin, "married_to", r(e.edwin), t(0)).unwrap();
    w.assert(e.edwin, "married_to", r(e.mara), t(1)).unwrap();
    w.assert(e.edwin, "married_to", r(e.lisa), t(2)).unwrap();
    let f = |a, b, from| ov(e.edwin, "married_to", Side::Forward, r(a), r(b), from, None);
    assert_eq!(
        overlaps(&w, "married_to"),
        vec![
            f(e.edwin, e.mara, 1),
            f(e.edwin, e.lisa, 2),
            f(e.mara, e.lisa, 2)
        ]
    );
}

fn bool_enc_order(x: bool, y: bool) -> (Value, Value) {
    let (a, b) = (Value::Bool(x), Value::Bool(y));
    if a.encode().unwrap() < b.encode().unwrap() {
        (a, b)
    } else {
        (b, a)
    }
}

#[test]
fn a_cardinality_one_edge_property_overlaps_on_the_edge_entity() {
    let (mut w, e) = mem();
    w.assert(e.edwin, "owns", r(e.sword), t(-300)).unwrap();
    let edge: EntityId = w.edge_id(e.edwin, "owns", e.sword).unwrap().into();
    w.assert(edge, "disguised", Value::Bool(true), t(-300))
        .unwrap();
    assert_eq!(overlaps(&w, "disguised"), vec![]);
    // replaced without ending the old value: both hold from -280
    w.assert(edge, "disguised", Value::Bool(false), t(-280))
        .unwrap();
    let (a, b) = bool_enc_order(true, false);
    let one = |until| {
        vec![Overlap {
            entity: edge,
            attr: "disguised".into(),
            side: Side::Forward,
            a: a.clone(),
            b: b.clone(),
            during: iv(-280, until),
        }]
    };
    assert_eq!(overlaps(&w, "disguised"), one(None));
    // the edge's own attribute (many) is untouched
    assert_eq!(overlaps(&w, "owns"), vec![]);
    // ending the old value at -250 bounds the overlap
    w.retract(edge, "disguised", Value::Bool(true), t(-250))
        .unwrap();
    assert_eq!(overlaps(&w, "disguised"), one(Some(-250)));
    // out-of-order: the old value also ended at -280, where the new one
    // starts: [-300,-280) and [-280,inf) touch, clean (the retract at -250 is
    // a harmless leftover)
    let r2 = w
        .retract(edge, "disguised", Value::Bool(true), t(-280))
        .unwrap();
    assert_eq!(r2.changed, 1);
    assert_eq!(overlaps(&w, "disguised"), vec![]);
}

#[test]
fn a_directed_attribute_with_an_inverse_name_reports_reverse_overlaps_under_its_own_name() {
    // a child has one parent: Edwin and Mara both parent_of Greg
    let (mut w, e) = mem();
    w.assert(e.edwin, "parent_of", r(e.greg), t(0)).unwrap();
    assert_eq!(overlaps(&w, "parent_of"), vec![]);
    w.assert(e.mara, "parent_of", r(e.greg), t(10)).unwrap();
    assert_eq!(
        overlaps(&w, "parent_of"),
        vec![ov(
            e.greg,
            "parent_of",
            Side::Reverse,
            r(e.edwin),
            r(e.mara),
            10,
            None
        )]
    );
    // forward parent_of is many: Edwin parenting two children is fine
    w.assert(e.edwin, "parent_of", r(e.lisa), t(0)).unwrap();
    assert_eq!(overlaps(&w, "parent_of").len(), 1);
    // Edwin stops at 10 (hand-over): touching, clean
    w.retract(e.edwin, "parent_of", r(e.greg), t(10)).unwrap();
    assert_eq!(overlaps(&w, "parent_of"), vec![]);
}

#[test]
fn older_canon_reverse_overlaps() {
    let (mut w, e) = mem();
    w.assert(e.edwin, "rules", r(e.carnath), t(10)).unwrap();
    let tx_one_ruler = w.latest_tx();
    let m = only(w.assert(e.mara, "rules", r(e.carnath), t(20)).unwrap());
    let tx_two = w.latest_tx();
    w.assert(e.lisa, "rules", r(e.carnath), t(30)).unwrap();
    let tx_three = w.latest_tx();
    w.correct(m, Correction::Delete).unwrap();
    let tx_fixed = w.latest_tx();
    let carnath = |a, b, from| ov(e.carnath, "rules", Side::Reverse, r(a), r(b), from, None);
    assert_eq!(w.at_canon(tx_one_ruler).overlaps("rules").unwrap(), vec![]);
    assert_eq!(
        w.at_canon(tx_two).overlaps("rules").unwrap(),
        vec![carnath(e.edwin, e.mara, 20)]
    );
    assert_eq!(
        w.at_canon(tx_three).overlaps("rules").unwrap(),
        vec![
            carnath(e.edwin, e.mara, 20),
            carnath(e.edwin, e.lisa, 30),
            carnath(e.mara, e.lisa, 30)
        ]
    );
    // deleting Mara's rule leaves Edwin and Lisa
    assert_eq!(
        w.at_canon(tx_fixed).overlaps("rules").unwrap(),
        vec![carnath(e.edwin, e.lisa, 30)]
    );
    assert_eq!(overlaps(&w, "rules"), vec![carnath(e.edwin, e.lisa, 30)]);
    // the reads are stable
    assert_eq!(
        w.at_canon(tx_two).overlaps("rules").unwrap(),
        vec![carnath(e.edwin, e.mara, 20)]
    );
}

#[test]
fn older_canon_overlaps_of_a_reopened_file_world_with_reverse_labels() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("w.kala");
    let mut w = World::create(&path, schema()).unwrap();
    let e = populate(&mut w);
    w.assert(e.edwin, "rules", r(e.carnath), t(10)).unwrap();
    let tx_one = w.latest_tx();
    w.assert(e.mara, "rules", r(e.carnath), t(20)).unwrap();
    let tx_two = w.latest_tx();
    w.retract(e.edwin, "rules", r(e.carnath), t(20)).unwrap();
    drop(w);
    let w = World::open(&path).unwrap();
    assert_eq!(w.at_canon(tx_one).overlaps("rules").unwrap(), vec![]);
    assert_eq!(
        w.at_canon(tx_two).overlaps("rules").unwrap(),
        vec![ov(
            e.carnath,
            "rules",
            Side::Reverse,
            r(e.edwin),
            r(e.mara),
            20,
            None
        )]
    );
    assert_eq!(w.overlaps("rules").unwrap(), vec![]);
}

// ---------------------------------------------------------------- crafted genesis: reverse_cardinality byte

#[allow(clippy::too_many_arguments)]
fn attr_body(
    id: u32,
    name: &str,
    vt: u8,
    kind: u8,
    card: u8,
    indexed: u8,
    reverse: u8,
    dir: u8,
    inverse: Option<&str>,
) -> Vec<u8> {
    let mut b = id.to_le_bytes().to_vec();
    b.extend_from_slice(&(name.len() as u32).to_le_bytes());
    b.extend_from_slice(name.as_bytes());
    b.extend_from_slice(&[vt, kind, card, indexed, reverse, dir]);
    match inverse {
        None => b.push(0),
        Some(i) => {
            b.push(1);
            b.extend_from_slice(&(i.len() as u32).to_le_bytes());
            b.extend_from_slice(i.as_bytes());
        }
    }
    b
}

fn genesis_file(attrs: &[Vec<u8>]) -> Vec<u8> {
    let mut body = Vec::new();
    let mut nt = 0u16.to_le_bytes().to_vec();
    nt.extend_from_slice(&9u32.to_le_bytes());
    nt.extend_from_slice(b"CHARACTER");
    body.extend(common::frame(1, 1, &nt));
    for (i, a) in attrs.iter().enumerate() {
        body.extend(common::frame(2, 2 + i as u64, a));
    }
    let commit = common::commit_frame(&body, 2 + attrs.len() as u64, 0);
    body.extend(commit);
    let mut file = common::header(kala_core::log::LOG_FORMAT_VERSION);
    file.extend(common::txn(0, &body));
    file
}

fn write_file(bytes: &[u8]) -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("w.kala");
    std::fs::write(&p, bytes).unwrap();
    (dir, p)
}

/// open and read_log both say Corrupt and leave the file alone.
fn assert_corrupt(attrs: &[Vec<u8>], label: &str) {
    let file = genesis_file(attrs);
    let (_d, p) = write_file(&file);
    match World::open(&p) {
        Err(Error::Corrupt(_)) => {}
        Err(e) => panic!("{label}: expected Corrupt, got {e:?}"),
        Ok(_) => panic!("{label}: expected Corrupt, open succeeded"),
    }
    assert_eq!(std::fs::read(&p).unwrap(), file, "{label}: file touched");
}

#[test]
fn a_reverse_cardinality_byte_of_three_is_corrupt_for_open_and_read_log() {
    for b in [3u8, 4, 255] {
        let attrs = [attr_body(0, "rules", 5, 0, 1, 1, b, 1, None)];
        assert_corrupt(&attrs, &format!("reverse byte {b}"));
        let (_d, p) = write_file(&genesis_file(&attrs));
        assert!(
            matches!(read_log(&p), Err(Error::Corrupt(_))),
            "read_log, byte {b}"
        );
    }
}

#[test]
fn reverse_one_on_a_bidirectional_attribute_is_corrupt_on_replay() {
    assert_corrupt(
        &[attr_body(0, "married_to", 5, 0, 0, 1, 1, 2, None)],
        "bidirectional + reverse one",
    );
}

#[test]
fn reverse_one_on_a_non_ref_attribute_is_corrupt_on_replay() {
    for vt in [1u8, 2, 3, 4] {
        assert_corrupt(
            &[attr_body(0, "x", vt, 0, 0, 1, 1, 0, None)],
            &format!("non-ref {vt} + reverse one"),
        );
    }
}

#[test]
fn control_a_hand_built_genesis_with_reverse_one_on_a_directed_ref_opens() {
    let file = genesis_file(&[
        attr_body(0, "rules", 5, 0, 1, 1, 1, 1, None),
        attr_body(1, "parent_of", 5, 0, 1, 1, 1, 1, Some("child_of")),
        attr_body(2, "owns", 5, 0, 1, 1, 0, 1, None),
    ]);
    let (_d, p) = write_file(&file);
    let w = World::open(&p).unwrap();
    drop(w);
    assert_eq!(std::fs::read(&p).unwrap(), file);
    let rc: Vec<_> = read_log(&p)
        .unwrap()
        .into_iter()
        .filter_map(|en| match en.body {
            EntryBody::DefineAttr { def, .. } => Some((def.name, def.reverse_cardinality)),
            _ => None,
        })
        .collect();
    assert_eq!(
        rc,
        vec![
            ("rules".to_string(), Some(Cardinality::One)),
            ("parent_of".to_string(), Some(Cardinality::One)),
            ("owns".to_string(), None),
        ]
    );
}

// ---------------------------------------------------------------- property tests (fixed seed)

fn cfg() -> ProptestConfig {
    ProptestConfig {
        cases: 256,
        rng_algorithm: RngAlgorithm::ChaCha,
        rng_seed: RngSeed::Fixed(0x6b61_6c61),
        failure_persistence: None,
        ..ProptestConfig::default()
    }
}

const LO: i64 = -6;
const HI: i64 = 6;

/// Per value (0..3): a set of distinct ticks in [-4, 4] with an op each,
/// flattened to history order (tick ascending, then value ascending).
fn changes_strategy() -> impl Strategy<Value = Vec<Change>> {
    proptest::collection::vec(
        proptest::collection::btree_map(-4i64..=4, any::<bool>(), 0..6),
        3,
    )
    .prop_map(|per_value| {
        let mut out: Vec<(i64, usize, bool)> = Vec::new();
        for (vi, m) in per_value.into_iter().enumerate() {
            for (tick, assert) in m {
                out.push((tick, vi, assert));
            }
        }
        out.sort();
        out.into_iter()
            .map(|(tick, vi, assert)| Change {
                tick: Tick(tick),
                op: if assert { Op::Assert } else { Op::Retract },
                value: Value::I64(vi as i64),
            })
            .collect()
    })
}

fn holds_brute(changes: &[Change], v: &Value, at: i64) -> bool {
    changes
        .iter()
        .filter(|c| &c.value == v && c.tick.0 <= at)
        .max_by_key(|c| c.tick)
        .is_some_and(|c| c.op == Op::Assert)
}

fn in_iv(i: &Interval, at: i64) -> bool {
    i.from.0 <= at && i.until.is_none_or(|u| at < u.0)
}

proptest! {
    #![proptest_config(cfg())]

    #[test]
    fn value_intervals_and_find_overlaps_match_a_per_tick_scan(changes in changes_strategy()) {
        let per_value = value_intervals(&changes);
        // shape: sorted values, ascending, disjoint, non-adjacent, non-empty
        for w2 in per_value.windows(2) {
            prop_assert!(w2[0].0.encode().unwrap() < w2[1].0.encode().unwrap());
        }
        for (_, ivs) in &per_value {
            prop_assert!(!ivs.is_empty());
            for i in ivs {
                if let Some(u) = i.until { prop_assert!(i.from < u, "empty interval {i:?}"); }
            }
            for p in ivs.windows(2) {
                let end = p[0].until.expect("only the last interval is open");
                prop_assert!(end < p[1].from, "touching or unordered: {ivs:?}");
            }
        }
        let values: Vec<Value> = (0..3).map(Value::I64).collect();
        let ivs_of = |v: &Value| -> Vec<Interval> {
            per_value.iter().find(|(x, _)| x == v).map(|(_, i)| i.clone()).unwrap_or_default()
        };
        for at in LO..=HI {
            for v in &values {
                let got = ivs_of(v).iter().any(|i| in_iv(i, at));
                prop_assert_eq!(got, holds_brute(&changes, v, at), "value {:?} at {}", v, at);
            }
        }
        // overlaps
        let rows = find_overlaps(&per_value);
        for (a, b, during) in &rows {
            prop_assert!(a.encode().unwrap() < b.encode().unwrap());
            if let Some(u) = during.until { prop_assert!(during.from < u); }
        }
        for at in LO..=HI {
            for (i, a) in values.iter().enumerate() {
                for b in &values[i + 1..] {
                    let want = holds_brute(&changes, a, at) && holds_brute(&changes, b, at);
                    let covering = rows.iter()
                        .filter(|(x, y, d)| x == a && y == b && in_iv(d, at))
                        .count();
                    prop_assert_eq!(covering, want as usize, "pair {:?}/{:?} at {}", a, b, at);
                }
            }
        }
        // maximal: rows of one pair neither touch nor overlap
        for (i, a) in values.iter().enumerate() {
            for b in &values[i + 1..] {
                let mut mine: Vec<_> = rows.iter().filter(|(x, y, _)| x == a && y == b).map(|r| r.2).collect();
                mine.sort_by_key(|d| d.from);
                for p in mine.windows(2) {
                    let end = p[0].until.expect("only the last row can be open");
                    prop_assert!(end < p[1].from, "rows touch: {mine:?}");
                }
            }
        }
    }
}

#[derive(Clone, Debug)]
struct W {
    entity: usize,
    place: usize,
    tick: i64,
    assert: bool,
}

fn writes_strategy() -> impl Strategy<Value = Vec<W>> {
    proptest::collection::vec(
        (0usize..2, 0usize..3, -4i64..=4, any::<bool>()).prop_map(
            |(entity, place, tick, assert)| W {
                entity,
                place,
                tick,
                assert,
            },
        ),
        0..24,
    )
}

proptest! {
    #![proptest_config(cfg())]

    /// World::overlaps vs a scan that asks the store's own as_of at every
    /// tick (independent of value_intervals / find_overlaps), over random
    /// writes on a cardinality-one attribute, at the latest and at every
    /// older canon.
    #[test]
    fn world_overlaps_match_a_per_tick_as_of_scan(ws in writes_strategy()) {
        let (mut w, e) = mem();
        let who = [e.edwin, e.greg];
        let places = [e.varrow, e.ashford, e.carnath];
        let mut canons = vec![w.latest_tx()];
        for x in &ws {
            let r = if x.assert {
                w.assert(who[x.entity], "located_in", Value::Ref(places[x.place]), t(x.tick))
            } else {
                w.retract(who[x.entity], "located_in", Value::Ref(places[x.place]), t(x.tick))
            };
            let _ = r; // same-tick conflicts are errors: ignored
            canons.push(w.latest_tx());
        }
        for canon in canons {
            let view = w.at_canon(canon);
            let rows = view.overlaps("located_in").unwrap();
            // sorted per contract
            for p in rows.windows(2) {
                prop_assert!(
                    (p[0].entity, p[0].side as u8, p[0].a.encode().unwrap(), p[0].b.encode().unwrap(), p[0].during.from)
                        <= (p[1].entity, p[1].side as u8, p[1].a.encode().unwrap(), p[1].b.encode().unwrap(), p[1].during.from)
                );
            }
            let mut want: BTreeSet<(u64, Vec<u8>, Vec<u8>, i64)> = BTreeSet::new();
            let mut got: BTreeSet<(u64, Vec<u8>, Vec<u8>, i64)> = BTreeSet::new();
            for ent in who {
                for at in LO..=HI {
                    let held = view_as_of(&view, ent, at);
                    for (i, a) in held.iter().enumerate() {
                        for b in &held[i + 1..] {
                            want.insert((ent.raw(), a.encode().unwrap(), b.encode().unwrap(), at));
                        }
                    }
                }
            }
            for o in &rows {
                prop_assert_eq!(o.side, Side::Forward);
                prop_assert!(o.a.encode().unwrap() < o.b.encode().unwrap());
                for at in LO..=HI {
                    if in_iv(&o.during, at) {
                        prop_assert!(got.insert((o.entity.raw(), o.a.encode().unwrap(), o.b.encode().unwrap(), at)), "duplicate cover {o:?} at {at}");
                    }
                }
            }
            prop_assert_eq!(got, want, "canon {:?}", canon);
        }
    }
}

/// The values held at `at` in `view`, sorted by encoding.
fn view_as_of(view: &View<'_>, ent: EntityId, at: i64) -> Vec<Value> {
    let mut v = view.as_of(ent, "located_in", Tick(at)).unwrap();
    v.sort_by_key(|x| x.encode().unwrap());
    v
}
