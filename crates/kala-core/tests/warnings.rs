//! Slice 3, item 7: write warnings (docs/slice-3-decisions.md, "Item 7" and
//! "Item 1" result shapes; CLAUDE.md "Attribute schema": the Greg
//! tx#50/#55/#57 example). RED until the implementation lands: this file
//! fails to COMPILE because `Warnings` and the `warnings` fields don't exist.
//!
//! API expected (everything re-exported at the crate root):
//!
//! ```text
//! #[derive(Clone, Debug, Default, PartialEq)]
//! pub struct Warnings { pub introduced: Vec<Overlap>, pub resolved: Vec<Overlap> }
//! WriteResult { .., pub warnings: Warnings }     // no longer Eq (Overlap holds Value/f64)
//! CommitResult { .., pub warnings: Warnings }
//! ```
//!
//! Contract being tested:
//! - auto-commit `World::assert/retract/correct`: `warnings` is that commit's
//!   diff (overlaps of the touched entities, before vs after, at the latest
//!   canon): `introduced` = after - before, `resolved` = before - after, by
//!   exact `Overlap` equality (a changed interval = resolved old +
//!   introduced new), each sorted by (entity, side, a, b, during.from).
//! - `Txn::assert/retract/correct` return `Warnings::default()`; the
//!   `CommitResult` carries the net diff of everything the txn touched.
//! - a no-op write / a no-op commit has empty warnings; warnings never block
//!   a write.
//!
//! Not pinned (flagged in the report): the relative order of overlaps of
//! different ATTRIBUTES on one entity within one commit (item 6's sort key
//! has no attribute); tests keep one attribute per commit where it matters.

use kala_core::*;
use proptest::prelude::*;
use proptest::test_runner::{RngAlgorithm, RngSeed};

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

fn r(e: EntityId) -> Value {
    Value::Ref(e)
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
        .attribute(AttrDef::new(
            "disguised",
            ValueType::Bool,
            Kind::State,
            Cardinality::One,
        ))
        .attribute(
            AttrDef::new("married_to", ValueType::Ref, Kind::State, Cardinality::One)
                .bidirectional(),
        )
        // a city has one ruler
        .attribute(
            AttrDef::new("rules", ValueType::Ref, Kind::State, Cardinality::Many).reverse_one(),
        )
        .attribute(
            AttrDef::new("wields", ValueType::Ref, Kind::State, Cardinality::One).reverse_one(),
        )
        // an inverse name: `child_of` reads the same stored edge
        .attribute(
            AttrDef::new("parent_of", ValueType::Ref, Kind::State, Cardinality::One)
                .inverse("child_of"),
        )
}

const ATTRS: [&str; 8] = [
    "located_in",
    "owns",
    "name",
    "born_in",
    "disguised",
    "married_to",
    "rules",
    "wields",
];

#[derive(Clone, Copy)]
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
    // creation order = id order inside a type: edwin < mara < lisa < greg;
    // varrow < ashford < carnath; sword < axe
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

/// A Forward overlap of `located_in` on `entity`.
fn loc(entity: EntityId, a: EntityId, b: EntityId, from: i64, until: Option<i64>) -> Overlap {
    ov(entity, "located_in", Side::Forward, r(a), r(b), from, until)
}

fn warns(introduced: Vec<Overlap>, resolved: Vec<Overlap>) -> Warnings {
    Warnings {
        introduced,
        resolved,
        ..Warnings::default()
    }
}

fn quiet() -> Warnings {
    Warnings::default()
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

/// Every overlap of every attribute at the latest canon.
fn all_overlaps(w: &World) -> Vec<Overlap> {
    ATTRS.iter().flat_map(|a| w.overlaps(a).unwrap()).collect()
}

fn sort_key(o: &Overlap) -> (u64, u8, Vec<u8>, Vec<u8>, Tick) {
    (
        o.entity.raw(),
        o.side as u8,
        o.a.encode().unwrap(),
        o.b.encode().unwrap(),
        o.during.from,
    )
}

fn assert_sorted(v: &[Overlap]) {
    for p in v.windows(2) {
        assert!(
            sort_key(&p[0]) <= sort_key(&p[1]),
            "not sorted: {:?} before {:?}",
            p[0],
            p[1]
        );
    }
}

/// `after == (before - resolved) + introduced` as sets, with `introduced`
/// disjoint from `before` and `resolved` disjoint from `after`.
fn assert_is_the_diff(before: &[Overlap], after: &[Overlap], wr: &Warnings) {
    assert_sorted(&wr.introduced);
    assert_sorted(&wr.resolved);
    for o in &wr.introduced {
        assert!(after.contains(o), "introduced but not in after: {o:?}");
        assert!(
            !before.contains(o),
            "introduced but already in before: {o:?}"
        );
    }
    for o in &wr.resolved {
        assert!(before.contains(o), "resolved but not in before: {o:?}");
        assert!(!after.contains(o), "resolved but still in after: {o:?}");
    }
    for o in after {
        assert!(
            before.contains(o) || wr.introduced.contains(o),
            "in after, not before, not introduced: {o:?}"
        );
    }
    for o in before {
        assert!(
            after.contains(o) || wr.resolved.contains(o),
            "in before, not after, not resolved: {o:?}"
        );
    }
}

// ---------------------------------------------------------------- the CLAUDE.md example

#[test]
fn greg_tx50_tx55_tx57() {
    let (mut w, e) = mem();
    // tx#50 Greg located_in Varrow @200: nothing to warn about
    let r50 = w.assert(e.greg, "located_in", r(e.varrow), t(200)).unwrap();
    assert_eq!(r50.changed, 1);
    assert_eq!(r50.tick, Tick(200));
    assert_eq!(r50.warnings, quiet());

    // tx#55 Greg located_in Ashford @601: "Greg is in Varrow and Ashford from 601"
    let r55 = w
        .assert(e.greg, "located_in", r(e.ashford), t(601))
        .unwrap();
    assert_eq!(r55.changed, 1);
    let the_overlap = loc(e.greg, e.varrow, e.ashford, 601, None);
    assert_eq!(r55.warnings, warns(vec![the_overlap.clone()], vec![]));
    // it is exactly what the overlap check (doctor's source) reports
    assert_eq!(w.overlaps("located_in").unwrap(), vec![the_overlap.clone()]);

    // tx#57 Greg located_in Varrow @250 retract: "warning gone"
    let r57 = w
        .retract(e.greg, "located_in", r(e.varrow), t(250))
        .unwrap();
    assert_eq!(r57.changed, 1);
    assert_eq!(r57.warnings, warns(vec![], vec![the_overlap]));
    assert_eq!(w.overlaps("located_in").unwrap(), vec![]);
}

#[test]
fn greg_example_on_a_file_world_and_after_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("w.kala");
    let mut w = World::create(&path, schema()).unwrap();
    let e = populate(&mut w);
    w.assert(e.greg, "located_in", r(e.varrow), t(200)).unwrap();
    drop(w);
    let mut w = World::open(&path).unwrap();
    let the_overlap = loc(e.greg, e.varrow, e.ashford, 601, None);
    let r55 = w
        .assert(e.greg, "located_in", r(e.ashford), t(601))
        .unwrap();
    assert_eq!(r55.warnings, warns(vec![the_overlap.clone()], vec![]));
    let r57 = w
        .retract(e.greg, "located_in", r(e.varrow), t(250))
        .unwrap();
    assert_eq!(r57.warnings, warns(vec![], vec![the_overlap]));
}

#[test]
fn the_pair_is_a_b_in_encoding_order_whichever_value_was_written_first() {
    let (mut w, e) = mem();
    w.assert(e.greg, "located_in", r(e.ashford), t(601))
        .unwrap();
    let wr = w.assert(e.greg, "located_in", r(e.varrow), t(200)).unwrap();
    // Ashford is open from 601; Varrow from 200 and never ended: overlap [601, inf)
    assert_eq!(
        wr.warnings,
        warns(vec![loc(e.greg, e.varrow, e.ashford, 601, None)], vec![])
    );
}

#[test]
fn a_non_ref_cardinality_one_attribute_warns_too() {
    let (mut w, e) = mem();
    w.assert(e.greg, "name", st("Greg"), t(0)).unwrap();
    let wr = w.assert(e.greg, "name", st("Gregor"), t(10)).unwrap();
    let o = ov(
        e.greg,
        "name",
        Side::Forward,
        st("Greg"),
        st("Gregor"),
        10,
        None,
    );
    assert_eq!(wr.warnings, warns(vec![o.clone()], vec![]));
    let wr = w.retract(e.greg, "name", st("Greg"), t(10)).unwrap();
    // retract at 10: Greg [0,10), Gregor [10,inf): touching
    assert_eq!(wr.warnings, warns(vec![], vec![o]));
}

// ---------------------------------------------------------------- out-of-order writes

#[test]
fn a_prequel_write_can_introduce_an_overlap_earlier_in_the_timeline() {
    let (mut w, e) = mem();
    w.assert(e.greg, "located_in", r(e.varrow), t(200)).unwrap();
    w.retract(e.greg, "located_in", r(e.varrow), t(250))
        .unwrap();
    let late = w
        .assert(e.greg, "located_in", r(e.ashford), t(601))
        .unwrap();
    assert_eq!(late.warnings, quiet());
    // written last, happened in the middle: Ashford holds from 220 on
    let wr = w
        .assert(e.greg, "located_in", r(e.ashford), t(220))
        .unwrap();
    assert_eq!(wr.changed, 1);
    assert_eq!(
        wr.warnings,
        warns(
            vec![loc(e.greg, e.varrow, e.ashford, 220, Some(250))],
            vec![]
        )
    );
}

#[test]
fn a_prequel_before_the_first_fact_overlaps_the_whole_first_stay() {
    let (mut w, e) = mem();
    w.assert(e.greg, "located_in", r(e.varrow), t(200)).unwrap();
    w.retract(e.greg, "located_in", r(e.varrow), t(250))
        .unwrap();
    let wr = w
        .assert(e.greg, "located_in", r(e.ashford), t(-50))
        .unwrap();
    assert_eq!(wr.tick, Tick(-50));
    assert_eq!(
        wr.warnings,
        warns(
            vec![loc(e.greg, e.varrow, e.ashford, 200, Some(250))],
            vec![]
        )
    );
}

#[test]
fn a_prequel_retract_resolves_an_existing_overlap() {
    let (mut w, e) = mem();
    w.assert(e.greg, "located_in", r(e.varrow), t(200)).unwrap();
    w.assert(e.greg, "located_in", r(e.ashford), t(601))
        .unwrap();
    // Varrow ended at 300 (he was there at 300, so this is stored)
    let wr = w
        .retract(e.greg, "located_in", r(e.varrow), t(300))
        .unwrap();
    assert_eq!(wr.changed, 1);
    assert_eq!(
        wr.warnings,
        warns(vec![], vec![loc(e.greg, e.varrow, e.ashford, 601, None)])
    );
}

#[test]
fn a_write_that_changes_an_overlaps_interval_resolves_the_old_and_introduces_the_new() {
    let (mut w, e) = mem();
    w.assert(e.greg, "located_in", r(e.varrow), t(200)).unwrap();
    w.assert(e.greg, "located_in", r(e.ashford), t(220))
        .unwrap();
    // Varrow ends at 250: [220, inf) becomes [220, 250)
    let wr = w
        .retract(e.greg, "located_in", r(e.varrow), t(250))
        .unwrap();
    assert_eq!(
        wr.warnings,
        warns(
            vec![loc(e.greg, e.varrow, e.ashford, 220, Some(250))],
            vec![loc(e.greg, e.varrow, e.ashford, 220, None)]
        )
    );
    // Ashford ends at 230: [220, 250) becomes [220, 230)
    let wr = w
        .retract(e.greg, "located_in", r(e.ashford), t(230))
        .unwrap();
    assert_eq!(
        wr.warnings,
        warns(
            vec![loc(e.greg, e.varrow, e.ashford, 220, Some(230))],
            vec![loc(e.greg, e.varrow, e.ashford, 220, Some(250))]
        )
    );
}

#[test]
fn a_write_can_split_one_overlap_into_two() {
    // Varrow [0, inf), Ashford [10, inf): overlap [10, inf).
    // Ashford retracted @20 and asserted again @30: [10,20) and [30,inf).
    let (mut w, e) = mem();
    w.assert(e.greg, "located_in", r(e.varrow), t(0)).unwrap();
    w.assert(e.greg, "located_in", r(e.ashford), t(10)).unwrap();
    let wr = w
        .retract(e.greg, "located_in", r(e.ashford), t(20))
        .unwrap();
    assert_eq!(
        wr.warnings,
        warns(
            vec![loc(e.greg, e.varrow, e.ashford, 10, Some(20))],
            vec![loc(e.greg, e.varrow, e.ashford, 10, None)]
        )
    );
    let wr = w.assert(e.greg, "located_in", r(e.ashford), t(30)).unwrap();
    assert_eq!(
        wr.warnings,
        warns(vec![loc(e.greg, e.varrow, e.ashford, 30, None)], vec![])
    );
}

#[test]
fn touching_intervals_produce_no_warning_in_either_write_order() {
    // retract first, then assert at the same tick: never overlapping
    let (mut w, e) = mem();
    w.assert(e.greg, "located_in", r(e.varrow), t(-300))
        .unwrap();
    let a = w
        .retract(e.greg, "located_in", r(e.varrow), t(-250))
        .unwrap();
    let b = w
        .assert(e.greg, "located_in", r(e.ashford), t(-250))
        .unwrap();
    assert_eq!((a.warnings, b.warnings), (quiet(), quiet()));
    assert_eq!((a.changed, b.changed), (1, 1));

    // assert first: Varrow is still open, so it warns, and the retract at the
    // same tick resolves exactly that warning
    let (mut w, e) = mem();
    w.assert(e.greg, "located_in", r(e.varrow), t(-300))
        .unwrap();
    let b = w
        .assert(e.greg, "located_in", r(e.ashford), t(-250))
        .unwrap();
    let o = loc(e.greg, e.varrow, e.ashford, -250, None);
    assert_eq!(b.warnings, warns(vec![o.clone()], vec![]));
    let a = w
        .retract(e.greg, "located_in", r(e.varrow), t(-250))
        .unwrap();
    assert_eq!(a.warnings, warns(vec![], vec![o]));
}

#[test]
fn a_gap_is_never_a_warning() {
    let (mut w, e) = mem();
    w.assert(e.greg, "located_in", r(e.varrow), t(200)).unwrap();
    let a = w
        .retract(e.greg, "located_in", r(e.varrow), t(250))
        .unwrap();
    let b = w
        .assert(e.greg, "located_in", r(e.ashford), t(601))
        .unwrap();
    assert_eq!((a.warnings, b.warnings), (quiet(), quiet()));
}

#[test]
fn extreme_ticks_warn_and_resolve_exactly() {
    let (mut w, e) = mem();
    w.assert(e.greg, "located_in", r(e.varrow), t(i64::MIN))
        .unwrap();
    let wr = w
        .assert(e.greg, "located_in", r(e.ashford), t(i64::MAX))
        .unwrap();
    let o = loc(e.greg, e.varrow, e.ashford, i64::MAX, None);
    assert_eq!(wr.warnings, warns(vec![o.clone()], vec![]));
    let wr = w
        .retract(e.greg, "located_in", r(e.varrow), t(i64::MAX))
        .unwrap();
    assert_eq!(wr.warnings, warns(vec![], vec![o]));
}

// ---------------------------------------------------------------- corrections

#[test]
fn deleting_the_overlapping_assert_resolves_the_warning() {
    let (mut w, e) = mem();
    w.assert(e.greg, "located_in", r(e.varrow), t(200)).unwrap();
    let ash = only(
        w.assert(e.greg, "located_in", r(e.ashford), t(220))
            .unwrap(),
    );
    let wr = w.correct(ash, Correction::Delete).unwrap();
    assert_eq!(wr.changed, 1);
    assert_eq!(
        wr.warnings,
        warns(vec![], vec![loc(e.greg, e.varrow, e.ashford, 220, None)])
    );
}

#[test]
fn deleting_a_retract_introduces_an_overlap() {
    let (mut w, e) = mem();
    w.assert(e.greg, "located_in", r(e.varrow), t(200)).unwrap();
    let rv = only(
        w.retract(e.greg, "located_in", r(e.varrow), t(250))
            .unwrap(),
    );
    w.assert(e.greg, "located_in", r(e.ashford), t(250))
        .unwrap();
    // the author decides Greg never left Varrow
    let wr = w.correct(rv, Correction::Delete).unwrap();
    assert_eq!(
        wr.warnings,
        warns(vec![loc(e.greg, e.varrow, e.ashford, 250, None)], vec![])
    );
}

#[test]
fn moving_a_retract_changes_the_overlap_interval() {
    let (mut w, e) = mem();
    w.assert(e.greg, "located_in", r(e.varrow), t(200)).unwrap();
    let rv = only(
        w.retract(e.greg, "located_in", r(e.varrow), t(250))
            .unwrap(),
    );
    w.assert(e.greg, "located_in", r(e.ashford), t(220))
        .unwrap();
    // [220, 250) -> [220, 230)
    let wr = w.correct(rv, rep(None, t(230), None)).unwrap();
    assert_eq!(wr.changed, 2);
    assert_eq!(
        wr.warnings,
        warns(
            vec![loc(e.greg, e.varrow, e.ashford, 220, Some(230))],
            vec![loc(e.greg, e.varrow, e.ashford, 220, Some(250))]
        )
    );
}

#[test]
fn moving_a_retract_to_touch_resolves_the_overlap() {
    let (mut w, e) = mem();
    w.assert(e.greg, "located_in", r(e.varrow), t(200)).unwrap();
    let rv = only(
        w.retract(e.greg, "located_in", r(e.varrow), t(250))
            .unwrap(),
    );
    w.assert(e.greg, "located_in", r(e.ashford), t(220))
        .unwrap();
    let wr = w.correct(rv, rep(None, t(220), None)).unwrap();
    assert_eq!(
        wr.warnings,
        warns(
            vec![],
            vec![loc(e.greg, e.varrow, e.ashford, 220, Some(250))]
        )
    );
}

#[test]
fn moving_an_assert_into_an_overlap_introduces_it() {
    let (mut w, e) = mem();
    w.assert(e.greg, "located_in", r(e.varrow), t(200)).unwrap();
    w.retract(e.greg, "located_in", r(e.varrow), t(250))
        .unwrap();
    let ash = only(
        w.assert(e.greg, "located_in", r(e.ashford), t(601))
            .unwrap(),
    );
    // "he was in Ashford from 240, not 601"
    let wr = w.correct(ash, rep(None, t(240), None)).unwrap();
    assert_eq!(
        wr.warnings,
        warns(
            vec![loc(e.greg, e.varrow, e.ashford, 240, Some(250))],
            vec![]
        )
    );
    // and back out of it
    let wr = w
        .correct(revised_of(&w, ash), rep(None, t(250), None))
        .unwrap();
    assert_eq!(
        wr.warnings,
        warns(
            vec![],
            vec![loc(e.greg, e.varrow, e.ashford, 240, Some(250))]
        )
    );
}

/// The newest entry of the chain that starts at `base`: scans the log
/// forward for the last `Revised` entry of it.
fn revised_of(w: &World, base: EntryId) -> EntryId {
    let mut last = base;
    let mut id = base.0 + 1;
    while let Some(info) = w.entry(EntryId(id)) {
        if let Some((of, _)) = info.revision()
            && of == base
            && matches!(info.body, log::EntryBody::Revised { .. })
        {
            last = info.id;
        }
        id += 1;
    }
    last
}

#[test]
fn replacing_the_value_moves_the_overlap_to_the_new_value() {
    let (mut w, e) = mem();
    w.assert(e.greg, "located_in", r(e.varrow), t(0)).unwrap();
    let x = only(w.assert(e.greg, "located_in", r(e.ashford), t(10)).unwrap());
    let wr = w.correct(x, rep(Some(r(e.carnath)), None, None)).unwrap();
    assert_eq!(
        wr.warnings,
        warns(
            vec![loc(e.greg, e.varrow, e.carnath, 10, None)],
            vec![loc(e.greg, e.varrow, e.ashford, 10, None)]
        )
    );
}

#[test]
fn replacing_the_op_of_an_entry_can_resolve_an_overlap() {
    // a retract recorded by mistake as an assert: Varrow @250 "assert" keeps
    // Varrow open; turning it into a retract ends it.
    let (mut w, e) = mem();
    w.assert(e.greg, "located_in", r(e.varrow), t(200)).unwrap();
    w.assert(e.greg, "located_in", r(e.ashford), t(220))
        .unwrap();
    let rv = only(
        w.retract(e.greg, "located_in", r(e.varrow), t(250))
            .unwrap(),
    );
    // currently [220, 250); replacing the retract by an assert (hold-based
    // no-op rules don't apply to corrections) makes Varrow hold forever
    let wr = w.correct(rv, rep(None, None, Some(Op::Assert))).unwrap();
    assert_eq!(wr.changed, 1);
    assert_eq!(
        wr.warnings,
        warns(
            vec![loc(e.greg, e.varrow, e.ashford, 220, None)],
            vec![loc(e.greg, e.varrow, e.ashford, 220, Some(250))]
        )
    );
}

#[test]
fn a_noop_correction_has_empty_warnings() {
    let (mut w, e) = mem();
    w.assert(e.greg, "located_in", r(e.varrow), t(200)).unwrap();
    let ash = only(
        w.assert(e.greg, "located_in", r(e.ashford), t(220))
            .unwrap(),
    );
    let wr = w.correct(ash, rep(None, t(220), None)).unwrap();
    assert_eq!(wr.changed, 0);
    assert_eq!(wr.warnings, quiet());
}

// ---------------------------------------------------------------- reverse one

#[test]
fn two_rulers_of_one_city_warn_on_the_reverse_side() {
    let (mut w, e) = mem();
    let a = w.assert(e.edwin, "rules", r(e.carnath), t(10)).unwrap();
    assert_eq!(a.warnings, quiet());
    let b = w.assert(e.mara, "rules", r(e.carnath), t(20)).unwrap();
    let o = ov(
        e.carnath,
        "rules",
        Side::Reverse,
        r(e.edwin),
        r(e.mara),
        20,
        None,
    );
    assert_eq!(b.warnings, warns(vec![o.clone()], vec![]));
    // a hand-over: Edwin stops at 20
    let c = w.retract(e.edwin, "rules", r(e.carnath), t(20)).unwrap();
    assert_eq!(c.warnings, warns(vec![], vec![o]));
}

#[test]
fn a_ruler_of_two_cities_is_not_a_warning_when_only_the_reverse_side_is_one() {
    let (mut w, e) = mem();
    w.assert(e.edwin, "rules", r(e.carnath), t(0)).unwrap();
    let wr = w.assert(e.edwin, "rules", r(e.varrow), t(0)).unwrap();
    assert_eq!(wr.warnings, quiet());
}

#[test]
fn a_third_ruler_introduces_only_the_new_pairs() {
    let (mut w, e) = mem();
    w.assert(e.edwin, "rules", r(e.carnath), t(10)).unwrap();
    w.assert(e.mara, "rules", r(e.carnath), t(20)).unwrap();
    let wr = w.assert(e.lisa, "rules", r(e.carnath), t(30)).unwrap();
    let c = |a, b, from| ov(e.carnath, "rules", Side::Reverse, r(a), r(b), from, None);
    // (edwin, mara) already existed and isn't re-reported
    assert_eq!(
        wr.warnings,
        warns(vec![c(e.edwin, e.lisa, 30), c(e.mara, e.lisa, 30)], vec![])
    );
}

#[test]
fn a_reverse_prequel_write_overlaps_the_past() {
    let (mut w, e) = mem();
    w.assert(e.edwin, "rules", r(e.carnath), t(10)).unwrap();
    w.retract(e.edwin, "rules", r(e.carnath), t(50)).unwrap();
    w.assert(e.mara, "rules", r(e.carnath), t(60)).unwrap();
    let wr = w.assert(e.mara, "rules", r(e.carnath), t(30)).unwrap();
    assert_eq!(
        wr.warnings,
        warns(
            vec![ov(
                e.carnath,
                "rules",
                Side::Reverse,
                r(e.edwin),
                r(e.mara),
                30,
                Some(50)
            )],
            vec![]
        )
    );
}

#[test]
fn a_correction_that_moves_the_target_touches_both_cities() {
    // Mara and Edwin both rule Carnath from 20; Mara's entry is corrected to
    // say she rules Varrow: Carnath's overlap resolves, Varrow has one ruler.
    let (mut w, e) = mem();
    w.assert(e.edwin, "rules", r(e.carnath), t(10)).unwrap();
    let m = only(w.assert(e.mara, "rules", r(e.carnath), t(20)).unwrap());
    let wr = w.correct(m, rep(Some(r(e.varrow)), None, None)).unwrap();
    assert_eq!(
        wr.warnings,
        warns(
            vec![],
            vec![ov(
                e.carnath,
                "rules",
                Side::Reverse,
                r(e.edwin),
                r(e.mara),
                20,
                None
            )]
        )
    );
}

#[test]
fn forward_and_reverse_overlaps_of_one_entity_come_forward_first() {
    // wields: forward one and reverse one. In ONE commit Greg gets a forward
    // overlap (sword + axe) and a reverse one (Edwin + Mara wield him).
    let (mut w, e) = mem();
    let mut tx = w.begin();
    tx.assert(e.mara, "wields", r(e.greg), t(0)).unwrap();
    tx.assert(e.edwin, "wields", r(e.greg), t(0)).unwrap();
    tx.assert(e.greg, "wields", r(e.axe), t(0)).unwrap();
    tx.assert(e.greg, "wields", r(e.sword), t(0)).unwrap();
    let c = tx.commit().unwrap();
    assert_eq!(
        c.warnings,
        warns(
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
            ],
            vec![]
        )
    );
}

// ---------------------------------------------------------------- bidirectional

fn married_to_setup(w: &mut World, e: &Ents) {
    // Edwin-Lisa and Mara-Greg already married from 0
    w.assert(e.edwin, "married_to", r(e.lisa), t(0)).unwrap();
    w.assert(e.mara, "married_to", r(e.greg), t(0)).unwrap();
}

fn both_ends_overlaps(e: &Ents) -> Vec<Overlap> {
    vec![
        // Edwin's partners {Lisa, Mara}; mara < lisa
        ov(
            e.edwin,
            "married_to",
            Side::Forward,
            r(e.mara),
            r(e.lisa),
            5,
            None,
        ),
        // Mara's partners {Edwin, Greg}; edwin < greg
        ov(
            e.mara,
            "married_to",
            Side::Forward,
            r(e.edwin),
            r(e.greg),
            5,
            None,
        ),
    ]
}

#[test]
fn a_bidirectional_write_reports_the_overlaps_of_both_entities() {
    let (mut w, e) = mem();
    married_to_setup(&mut w, &e);
    let wr = w.assert(e.edwin, "married_to", r(e.mara), t(5)).unwrap();
    assert_eq!(wr.changed, 1);
    assert_eq!(wr.warnings, warns(both_ends_overlaps(&e), vec![]));
}

#[test]
fn the_other_spelling_of_a_bidirectional_write_gives_the_same_warnings() {
    let (mut w, e) = mem();
    married_to_setup(&mut w, &e);
    let wr = w.assert(e.mara, "married_to", r(e.edwin), t(5)).unwrap();
    assert_eq!(wr.changed, 1);
    assert_eq!(wr.warnings, warns(both_ends_overlaps(&e), vec![]));
}

#[test]
fn a_bidirectional_retract_later_changes_both_ends_intervals() {
    for from_mara in [false, true] {
        let (mut w, e) = mem();
        married_to_setup(&mut w, &e);
        w.assert(e.edwin, "married_to", r(e.mara), t(5)).unwrap();
        // the marriage ends at 9: both overlaps become [5, 9)
        let wr = if from_mara {
            w.retract(e.mara, "married_to", r(e.edwin), t(9))
        } else {
            w.retract(e.edwin, "married_to", r(e.mara), t(9))
        }
        .unwrap();
        assert_eq!(wr.changed, 1, "from_mara {from_mara}");
        // the (mara|lisa) and (edwin|greg) pairs are still [5, inf) -> no:
        // Edwin's {Lisa, Mara}: Mara now ends at 9 -> [5, 9); same for Mara's.
        let want_new = vec![
            ov(
                e.edwin,
                "married_to",
                Side::Forward,
                r(e.mara),
                r(e.lisa),
                5,
                Some(9),
            ),
            ov(
                e.mara,
                "married_to",
                Side::Forward,
                r(e.edwin),
                r(e.greg),
                5,
                Some(9),
            ),
        ];
        assert_eq!(
            wr.warnings,
            warns(want_new, both_ends_overlaps(&e)),
            "from_mara {from_mara}"
        );
    }
}

#[test]
fn a_bidirectional_write_that_already_holds_is_a_noop_with_empty_warnings() {
    let (mut w, e) = mem();
    married_to_setup(&mut w, &e);
    w.assert(e.edwin, "married_to", r(e.mara), t(5)).unwrap();
    let wr = w.assert(e.mara, "married_to", r(e.edwin), t(5)).unwrap();
    assert_eq!(wr.changed, 0);
    assert_eq!(wr.warnings, quiet());
}

#[test]
fn a_self_pair_plus_a_partner_warns_on_the_entity_itself() {
    let (mut w, e) = mem();
    let a = w.assert(e.edwin, "married_to", r(e.edwin), t(0)).unwrap();
    assert_eq!(a.warnings, quiet());
    let b = w.assert(e.edwin, "married_to", r(e.mara), t(5)).unwrap();
    assert_eq!(
        b.warnings,
        warns(
            vec![ov(
                e.edwin,
                "married_to",
                Side::Forward,
                r(e.edwin),
                r(e.mara),
                5,
                None
            )],
            vec![]
        )
    );
}

// ---------------------------------------------------------------- edge properties

#[test]
fn a_cardinality_one_edge_property_warns_on_the_edge_entity() {
    let (mut w, e) = mem();
    w.assert(e.edwin, "owns", r(e.sword), t(-300)).unwrap();
    let edge: EntityId = w.edge_id(e.edwin, "owns", e.sword).unwrap().into();
    let a = w
        .assert(edge, "disguised", Value::Bool(true), t(-300))
        .unwrap();
    assert_eq!(a.warnings, quiet());
    let b = w
        .assert(edge, "disguised", Value::Bool(false), t(-280))
        .unwrap();
    assert_eq!(b.warnings.resolved, vec![]);
    assert_eq!(b.warnings.introduced.len(), 1);
    let o = &b.warnings.introduced[0];
    assert_eq!(o.entity, edge);
    assert_eq!(o.attr, "disguised");
    assert_eq!(o.side, Side::Forward);
    assert_eq!(o.during, iv(-280, None));
}

// ---------------------------------------------------------------- no warning

#[test]
fn cardinality_many_and_events_never_warn() {
    let (mut w, e) = mem();
    // many
    w.assert(e.greg, "owns", r(e.sword), t(0)).unwrap();
    assert_eq!(
        w.assert(e.greg, "owns", r(e.axe), t(5)).unwrap().warnings,
        quiet()
    );
    // events: the cardinality label is irrelevant, they never end
    w.assert(e.greg, "born_in", r(e.varrow), t(0)).unwrap();
    assert_eq!(
        w.assert(e.greg, "born_in", r(e.ashford), t(5))
            .unwrap()
            .warnings,
        quiet()
    );
    w.assert(e.greg, "bought", r(e.sword), t(0)).unwrap();
    assert_eq!(
        w.assert(e.greg, "bought", r(e.axe), t(0)).unwrap().warnings,
        quiet()
    );
}

#[test]
fn a_noop_write_has_empty_warnings_even_while_an_overlap_exists() {
    let (mut w, e) = mem();
    w.assert(e.greg, "located_in", r(e.varrow), t(200)).unwrap();
    w.assert(e.greg, "located_in", r(e.ashford), t(601))
        .unwrap();
    assert_eq!(w.overlaps("located_in").unwrap().len(), 1);
    // already held at 700
    let a = w
        .assert(e.greg, "located_in", r(e.ashford), t(700))
        .unwrap();
    assert_eq!(a.changed, 0);
    assert_eq!(a.warnings, quiet());
    // not held at 100
    let b = w
        .retract(e.greg, "located_in", r(e.ashford), t(100))
        .unwrap();
    assert_eq!(b.changed, 0);
    assert_eq!(b.warnings, quiet());
}

#[test]
fn writes_on_an_unrelated_entity_or_attribute_do_not_re_report_an_overlap() {
    let (mut w, e) = mem();
    w.assert(e.greg, "located_in", r(e.varrow), t(200)).unwrap();
    w.assert(e.greg, "located_in", r(e.ashford), t(601))
        .unwrap();
    // another entity, same attribute and same place
    let a = w.assert(e.edwin, "located_in", r(e.varrow), t(0)).unwrap();
    assert_eq!((a.changed, a.warnings), (1, quiet()));
    // the same entity, another attribute
    let b = w.assert(e.greg, "name", st("Greg"), t(0)).unwrap();
    assert_eq!((b.changed, b.warnings), (1, quiet()));
    let c = w.assert(e.greg, "owns", r(e.sword), t(0)).unwrap();
    assert_eq!((c.changed, c.warnings), (1, quiet()));
    // reverse side: Carnath has two rulers; Varrow's single ruler is unrelated
    w.assert(e.edwin, "rules", r(e.carnath), t(10)).unwrap();
    w.assert(e.mara, "rules", r(e.carnath), t(20)).unwrap();
    let d = w.assert(e.lisa, "rules", r(e.varrow), t(0)).unwrap();
    assert_eq!((d.changed, d.warnings), (1, quiet()));
    // Edwin rules a second city: his own (forward many) side isn't checked and
    // Carnath's existing overlap isn't re-reported
    let f = w.assert(e.edwin, "rules", r(e.ashford), t(0)).unwrap();
    assert_eq!((f.changed, f.warnings), (1, quiet()));
}

#[test]
fn warnings_never_block_the_write() {
    let (mut w, e) = mem();
    w.assert(e.greg, "located_in", r(e.varrow), t(200)).unwrap();
    let wr = w
        .assert(e.greg, "located_in", r(e.ashford), t(601))
        .unwrap();
    assert_eq!(wr.changed, 1);
    assert!(!wr.warnings.introduced.is_empty());
    // stored: both hold at 700, and the commit is in the log
    assert_eq!(
        w.as_of(e.greg, "located_in", Tick(700)).unwrap().len(),
        2,
        "both values hold"
    );
    assert!(w.entry(wr.entries[0]).is_some());
}

// ---------------------------------------------------------------- transactions

#[test]
fn txn_write_results_carry_no_warnings_and_the_commit_carries_the_diff() {
    let (mut w, e) = mem();
    let mut tx = w.begin();
    let a = tx
        .assert(e.greg, "located_in", r(e.varrow), t(200))
        .unwrap();
    let b = tx
        .assert(e.greg, "located_in", r(e.ashford), t(601))
        .unwrap();
    assert_eq!((a.changed, b.changed), (1, 1));
    assert_eq!(a.warnings, quiet());
    assert_eq!(b.warnings, quiet(), "the overlap is only known at commit");
    let c = tx.commit().unwrap();
    assert_eq!(c.changed, 2);
    assert_eq!(c.entries.len(), 2);
    assert_eq!(
        c.warnings,
        warns(vec![loc(e.greg, e.varrow, e.ashford, 601, None)], vec![])
    );
}

#[test]
fn a_commit_reports_the_net_diff_across_several_entities() {
    let (mut w, e) = mem();
    // existing: Greg overlaps from 601
    w.assert(e.greg, "located_in", r(e.varrow), t(200)).unwrap();
    w.assert(e.greg, "located_in", r(e.ashford), t(601))
        .unwrap();
    let mut tx = w.begin();
    // resolves Greg's overlap
    tx.retract(e.greg, "located_in", r(e.varrow), t(250))
        .unwrap();
    // introduces Edwin's
    tx.assert(e.edwin, "located_in", r(e.varrow), t(0)).unwrap();
    tx.assert(e.edwin, "located_in", r(e.carnath), t(10))
        .unwrap();
    let c = tx.commit().unwrap();
    assert_eq!(c.changed, 3);
    assert_eq!(
        c.warnings,
        warns(
            vec![loc(e.edwin, e.varrow, e.carnath, 10, None)],
            vec![loc(e.greg, e.varrow, e.ashford, 601, None)]
        )
    );
}

#[test]
fn an_overlap_made_and_resolved_in_one_txn_is_not_reported() {
    let (mut w, e) = mem();
    let mut tx = w.begin();
    tx.assert(e.greg, "located_in", r(e.varrow), t(200))
        .unwrap();
    tx.assert(e.greg, "located_in", r(e.ashford), t(601))
        .unwrap();
    tx.retract(e.greg, "located_in", r(e.varrow), t(250))
        .unwrap();
    let c = tx.commit().unwrap();
    assert_eq!(c.changed, 3);
    assert_eq!(c.warnings, quiet());
    assert_eq!(w.overlaps("located_in").unwrap(), vec![]);
}

#[test]
fn a_commit_that_changes_an_overlap_interval_reports_old_and_new() {
    let (mut w, e) = mem();
    w.assert(e.greg, "located_in", r(e.varrow), t(200)).unwrap();
    w.assert(e.greg, "located_in", r(e.ashford), t(220))
        .unwrap();
    let mut tx = w.begin();
    tx.retract(e.greg, "located_in", r(e.varrow), t(250))
        .unwrap();
    let c = tx.commit().unwrap();
    assert_eq!(
        c.warnings,
        warns(
            vec![loc(e.greg, e.varrow, e.ashford, 220, Some(250))],
            vec![loc(e.greg, e.varrow, e.ashford, 220, None)]
        )
    );
}

#[test]
fn a_txn_correct_returns_default_warnings_and_the_commit_the_diff() {
    let (mut w, e) = mem();
    w.assert(e.greg, "located_in", r(e.varrow), t(200)).unwrap();
    let ash = only(
        w.assert(e.greg, "located_in", r(e.ashford), t(220))
            .unwrap(),
    );
    let mut tx = w.begin();
    let wr = tx.correct(ash, Correction::Delete).unwrap();
    assert_eq!(wr.changed, 1);
    assert_eq!(wr.warnings, quiet());
    let c = tx.commit().unwrap();
    assert_eq!(
        c.warnings,
        warns(vec![], vec![loc(e.greg, e.varrow, e.ashford, 220, None)])
    );
}

#[test]
fn a_txn_that_creates_an_entity_and_overlaps_it() {
    let (mut w, e) = mem();
    let mut tx = w.begin();
    let x = tx.create_entity("CHARACTER", Some("x")).unwrap();
    tx.assert(x, "located_in", r(e.varrow), t(0)).unwrap();
    tx.assert(x, "located_in", r(e.carnath), t(5)).unwrap();
    let c = tx.commit().unwrap();
    assert_eq!(
        c.warnings,
        warns(vec![loc(x, e.varrow, e.carnath, 5, None)], vec![])
    );
}

#[test]
fn no_op_and_empty_commits_have_empty_warnings() {
    let (mut w, e) = mem();
    w.assert(e.greg, "located_in", r(e.varrow), t(200)).unwrap();
    w.assert(e.greg, "located_in", r(e.ashford), t(601))
        .unwrap(); // an existing overlap stays out of it
    let tx_before = w.latest_tx();
    // empty
    let c = w.begin().commit().unwrap();
    assert_eq!(c.tx, tx_before);
    assert_eq!(c.changed, 0);
    assert!(c.entries.is_empty() && c.edges.is_empty());
    assert_eq!(c.warnings, quiet());
    // only no-ops
    let mut tx = w.begin();
    let a = tx
        .assert(e.greg, "located_in", r(e.varrow), t(300))
        .unwrap();
    let b = tx
        .retract(e.greg, "located_in", r(e.carnath), t(300))
        .unwrap();
    assert_eq!((a.changed, b.changed), (0, 0));
    let c = tx.commit().unwrap();
    assert_eq!(c.tx, tx_before);
    assert_eq!(c.changed, 0);
    assert_eq!(c.warnings, quiet());
    assert_eq!(w.latest_tx(), tx_before);
}

#[test]
fn a_txn_with_a_failed_write_has_no_commit_and_the_world_keeps_its_warnings_clean() {
    let (mut w, e) = mem();
    let mut tx = w.begin();
    tx.assert(e.greg, "located_in", r(e.varrow), t(0)).unwrap();
    assert!(tx.assert(e.greg, "nope", r(e.varrow), t(0)).is_err());
    assert!(tx.commit().is_err());
    assert_eq!(all_overlaps(&w), vec![]);
}

// ---------------------------------------------------------------- sorting (item 6 order)

#[test]
fn introduced_overlaps_are_sorted_by_entity_side_a_b_and_start() {
    let (mut w, e) = mem();
    let mut tx = w.begin();
    // Greg (a later id) is written first
    tx.assert(e.greg, "located_in", r(e.ashford), t(0)).unwrap();
    tx.assert(e.greg, "located_in", r(e.varrow), t(0)).unwrap();
    tx.assert(e.edwin, "located_in", r(e.carnath), t(8))
        .unwrap();
    tx.assert(e.edwin, "located_in", r(e.ashford), t(5))
        .unwrap();
    tx.assert(e.edwin, "located_in", r(e.varrow), t(0)).unwrap();
    let c = tx.commit().unwrap();
    assert_eq!(
        c.warnings,
        warns(
            vec![
                loc(e.edwin, e.varrow, e.ashford, 5, None),
                loc(e.edwin, e.varrow, e.carnath, 8, None),
                loc(e.edwin, e.ashford, e.carnath, 8, None),
                loc(e.greg, e.varrow, e.ashford, 0, None),
            ],
            vec![]
        )
    );
}

#[test]
fn resolved_overlaps_are_sorted_the_same_way() {
    let (mut w, e) = mem();
    w.assert(e.greg, "located_in", r(e.varrow), t(0)).unwrap();
    let g_ash = only(w.assert(e.greg, "located_in", r(e.ashford), t(0)).unwrap());
    w.assert(e.edwin, "located_in", r(e.varrow), t(0)).unwrap();
    let e_ash = only(w.assert(e.edwin, "located_in", r(e.ashford), t(5)).unwrap());
    let e_car = only(w.assert(e.edwin, "located_in", r(e.carnath), t(8)).unwrap());
    let mut tx = w.begin();
    tx.correct(g_ash, Correction::Delete).unwrap();
    tx.correct(e_car, Correction::Delete).unwrap();
    tx.correct(e_ash, Correction::Delete).unwrap();
    let c = tx.commit().unwrap();
    assert_eq!(
        c.warnings,
        warns(
            vec![],
            vec![
                loc(e.edwin, e.varrow, e.ashford, 5, None),
                loc(e.edwin, e.varrow, e.carnath, 8, None),
                loc(e.edwin, e.ashford, e.carnath, 8, None),
                loc(e.greg, e.varrow, e.ashford, 0, None),
            ]
        )
    );
}

#[test]
fn one_pair_with_two_stretches_is_sorted_by_start() {
    // Varrow {[0,10), [20,inf)}, Ashford {[5,inf)}: [5,10) and [20,inf)
    let (mut w, e) = mem();
    let mut tx = w.begin();
    tx.assert(e.greg, "located_in", r(e.varrow), t(20)).unwrap();
    tx.assert(e.greg, "located_in", r(e.varrow), t(0)).unwrap();
    tx.retract(e.greg, "located_in", r(e.varrow), t(10))
        .unwrap();
    tx.assert(e.greg, "located_in", r(e.ashford), t(5)).unwrap();
    let c = tx.commit().unwrap();
    assert_eq!(
        c.warnings,
        warns(
            vec![
                loc(e.greg, e.varrow, e.ashford, 5, Some(10)),
                loc(e.greg, e.varrow, e.ashford, 20, None),
            ],
            vec![]
        )
    );
}

// ---------------------------------------------------------------- property: warnings are the diff of the full overlap set

#[derive(Clone, Debug)]
enum Op3 {
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
}

fn op3() -> impl Strategy<Value = Op3> {
    prop_oneof![
        4 => (0u8..3, 0u8..3, 0u8..3, -3i64..=3).prop_map(|(attr, who, target, tick)| Op3::Assert { attr, who, target, tick }),
        3 => (0u8..3, 0u8..3, 0u8..3, -3i64..=3).prop_map(|(attr, who, target, tick)| Op3::Retract { attr, who, target, tick }),
        1 => (0u8..32).prop_map(|nth| Op3::Delete { nth }),
        2 => (0u8..32, -3i64..=3).prop_map(|(nth, tick)| Op3::MoveTick { nth, tick }),
        1 => (0u8..32, 0u8..3).prop_map(|(nth, target)| Op3::MoveValue { nth, target }),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 128,
        rng_algorithm: RngAlgorithm::ChaCha,
        rng_seed: RngSeed::Fixed(0x7761_726e),
        failure_persistence: None,
        ..ProptestConfig::default()
    })]

    /// For every single write or correction: `introduced` = overlaps after
    /// minus before, `resolved` = before minus after, over ALL attributes
    /// (one write touches one attribute, so the touched-entities diff equals
    /// the global diff), each sorted. Attributes: located_in (forward one,
    /// ref), married_to (bidirectional one), rules (reverse one).
    #[test]
    fn every_write_reports_exactly_the_overlap_diff(ops in proptest::collection::vec(op3(), 0..28)) {
        let (mut w, e) = mem();
        let people = [e.edwin, e.mara, e.lisa];
        let places = [e.varrow, e.ashford, e.carnath];
        let attr_of = |a: u8| ["located_in", "married_to", "rules"][a as usize];
        // target pools per attribute: located_in/rules -> places; married_to -> people
        let target_of = |a: u8, i: u8| if a == 1 { r(people[i as usize]) } else { r(places[i as usize]) };
        // (entry, attribute index) of every entry written so far, to correct later
        let mut entries: Vec<(EntryId, u8)> = Vec::new();
        for op in ops {
            let before = all_overlaps(&w);
            let pick = |entries: &Vec<(EntryId, u8)>, nth: u8| {
                if entries.is_empty() { None } else { Some(entries[nth as usize % entries.len()]) }
            };
            let (res, attr_idx): (Result<WriteResult, Error>, u8) = match op {
                Op3::Assert { attr, who, target, tick } => (
                    w.assert(people[who as usize], attr_of(attr), target_of(attr, target), t(tick)),
                    attr,
                ),
                Op3::Retract { attr, who, target, tick } => (
                    w.retract(people[who as usize], attr_of(attr), target_of(attr, target), t(tick)),
                    attr,
                ),
                Op3::Delete { nth } => match pick(&entries, nth) {
                    Some((id, a)) => (w.correct(id, Correction::Delete), a),
                    None => continue,
                },
                Op3::MoveTick { nth, tick } => match pick(&entries, nth) {
                    Some((id, a)) => (w.correct(id, rep(None, t(tick), None)), a),
                    None => continue,
                },
                Op3::MoveValue { nth, target } => match pick(&entries, nth) {
                    Some((id, a)) => (w.correct(id, rep(Some(target_of(a, target)), None, None)), a),
                    None => continue,
                },
            };
            let after = all_overlaps(&w);
            match res {
                Ok(wr) => {
                    entries.extend(wr.entries.iter().map(|&id| (id, attr_idx)));
                    assert_is_the_diff(&before, &after, &wr.warnings);
                    if wr.changed == 0 {
                        prop_assert_eq!(wr.warnings, quiet());
                    }
                }
                Err(_) => {
                    // a refused write changes nothing and reports nothing
                    prop_assert!(before == after);
                }
            }
        }
    }
}

// ---------------------------------------------------------------- slice 3 confirm additions

#[test]
fn overlaps_of_an_inverse_name_is_an_unknown_attribute() {
    let (mut w, e) = mem();
    // an overlap exists on the real name; the inverse name is still not an attribute
    w.assert(e.edwin, "parent_of", r(e.mara), t(0)).unwrap();
    w.assert(e.edwin, "parent_of", r(e.lisa), t(0)).unwrap();
    assert_eq!(w.overlaps("parent_of").unwrap().len(), 1);
    assert_eq!(
        w.overlaps("child_of"),
        Err(Error::UnknownAttribute("child_of".into()))
    );
    let tx = w.latest_tx();
    assert_eq!(
        w.at_canon(tx).overlaps("child_of"),
        Err(Error::UnknownAttribute("child_of".into()))
    );
}

#[test]
fn a_multi_attribute_commit_is_sorted_by_entity_then_attr_name_then_side() {
    // PM decision: (entity, attr name, side, a, b, during.from). Writes are
    // issued in the reverse of the expected order.
    let (mut w, e) = mem();
    let mut tx = w.begin();
    tx.assert(e.greg, "wields", r(e.axe), t(0)).unwrap();
    tx.assert(e.greg, "wields", r(e.sword), t(0)).unwrap();
    tx.assert(e.mara, "wields", r(e.greg), t(0)).unwrap();
    tx.assert(e.edwin, "wields", r(e.greg), t(0)).unwrap();
    tx.assert(e.greg, "name", st("B"), t(0)).unwrap();
    tx.assert(e.greg, "name", st("A"), t(0)).unwrap();
    tx.assert(e.greg, "located_in", r(e.ashford), t(0)).unwrap();
    tx.assert(e.greg, "located_in", r(e.varrow), t(0)).unwrap();
    tx.assert(e.edwin, "name", st("Y"), t(0)).unwrap();
    tx.assert(e.edwin, "name", st("X"), t(0)).unwrap();
    let c = tx.commit().unwrap();
    assert_eq!(
        c.warnings,
        warns(
            vec![
                ov(e.edwin, "name", Side::Forward, st("X"), st("Y"), 0, None),
                loc(e.greg, e.varrow, e.ashford, 0, None),
                ov(e.greg, "name", Side::Forward, st("A"), st("B"), 0, None),
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
            ],
            vec![]
        )
    );
}

#[test]
fn a_bidirectional_correction_moving_a_pair_to_a_new_partner_touches_all_three() {
    let (mut w, e) = mem();
    married_to_setup(&mut w, &e); // Edwin-Lisa @0, Mara-Greg @0
    let x = only(w.assert(e.edwin, "married_to", r(e.mara), t(5)).unwrap());
    // Edwin-Mara is corrected to Edwin-Greg: Edwin and Mara lose the old
    // pair, Greg (who is married to Mara from 0) gains Edwin from 5.
    let wr = w.correct(x, rep(Some(r(e.greg)), None, None)).unwrap();
    assert_eq!(wr.changed, 2);
    let mm = |en: EntityId, a: EntityId, b: EntityId| {
        ov(en, "married_to", Side::Forward, r(a), r(b), 5, None)
    };
    assert_eq!(
        wr.warnings,
        warns(
            vec![mm(e.edwin, e.lisa, e.greg), mm(e.greg, e.edwin, e.mara)],
            vec![mm(e.edwin, e.mara, e.lisa), mm(e.mara, e.edwin, e.greg)],
        )
    );
}

#[test]
fn a_replace_onto_a_key_that_already_holds_the_same_op_reports_the_delete_only() {
    let (mut w, e) = mem();
    w.assert(e.greg, "located_in", r(e.varrow), t(0)).unwrap();
    w.assert(e.greg, "located_in", r(e.ashford), t(10)).unwrap();
    w.retract(e.greg, "located_in", r(e.ashford), t(20))
        .unwrap();
    w.assert(e.greg, "located_in", r(e.ashford), t(25)).unwrap();
    let r30 = only(
        w.retract(e.greg, "located_in", r(e.ashford), t(30))
            .unwrap(),
    );
    // Ashford: [10,20) and [25,30): Varrow overlaps both.
    assert_eq!(w.overlaps("located_in").unwrap().len(), 2);
    // Moving the retract to 20, where a retract is already current: only the
    // DELETED at 30 is written, so Ashford becomes [10,20) and [25,inf).
    let wr = w.correct(r30, rep(None, t(20), None)).unwrap();
    assert_eq!((wr.changed, wr.tick), (1, Tick(20)));
    assert_eq!(wr.entries.len(), 1);
    assert!(matches!(
        w.entry(wr.entries[0]).unwrap().body,
        log::EntryBody::Deleted { .. }
    ));
    assert_eq!(
        wr.warnings,
        warns(
            vec![loc(e.greg, e.varrow, e.ashford, 25, None)],
            vec![loc(e.greg, e.varrow, e.ashford, 25, Some(30))]
        )
    );
}

#[test]
fn a_failed_sync_then_reopen_the_next_commits_warnings_do_not_count_the_lost_txn() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("w.kala");
    let mut w = World::create(&path, schema()).unwrap();
    let e = populate(&mut w);
    w.assert(e.greg, "located_in", r(e.varrow), t(200)).unwrap();
    w.inject_log_fault(LogFault::FailNextSync);
    assert!(
        w.assert(e.greg, "located_in", r(e.ashford), t(601))
            .is_err()
    );
    drop(w);
    let mut w = World::open(&path).unwrap();
    assert_eq!(w.overlaps("located_in").unwrap(), vec![]);
    let wr = w
        .assert(e.greg, "located_in", r(e.carnath), t(700))
        .unwrap();
    // only the Varrow/Carnath pair: the lost Ashford write is not counted
    assert_eq!(
        wr.warnings,
        warns(vec![loc(e.greg, e.varrow, e.carnath, 700, None)], vec![])
    );
    assert_eq!(w.overlaps("located_in").unwrap().len(), 1);
}
