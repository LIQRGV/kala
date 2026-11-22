//! Slice 4 item 2, confirm round: edge cases of the case-insensitive V index.
//!
//! - two values that fold equal, written at the same tick with opposite ops on
//!   one entity, are different values: no same-tick conflict, separate histories.
//! - an edge property is still found by `lookup_ci` after its edge triple is
//!   retracted (properties are independent facts).
//! - folds that are byte prefixes of each other, or contain NUL, never over-match.
//! - the fold of "" is "" and only "" finds it.

use kala_core::*;

fn t(x: i64) -> Option<Tick> {
    Some(Tick(x))
}

fn s(x: &str) -> Value {
    Value::Str(x.into())
}

fn world() -> (World, EntityId, EntityId, EntityId) {
    let schema = Schema::new()
        .node_type("CHARACTER")
        .node_type("PLACE")
        .attribute(AttrDef::new(
            "name",
            ValueType::Str,
            Kind::State,
            Cardinality::Many,
        ))
        .attribute(AttrDef::new(
            "note",
            ValueType::Str,
            Kind::State,
            Cardinality::Many,
        ))
        .attribute(AttrDef::new(
            "present",
            ValueType::Ref,
            Kind::State,
            Cardinality::Many,
        ));
    let mut w = World::in_memory(schema).unwrap();
    let a = w.create_entity("CHARACTER", Some("a")).unwrap();
    let b = w.create_entity("CHARACTER", Some("b")).unwrap();
    let p = w.create_entity("PLACE", Some("p")).unwrap();
    (w, a, b, p)
}

fn ci(w: &World, attr: &str, text: &str, at: i64) -> Vec<EntityId> {
    w.lookup_ci(attr, text, Tick(at)).unwrap()
}

fn clean(w: &World) {
    let rep = doctor(w, &DoctorOptions::default());
    assert!(rep.is_clean(), "{:#?}", rep.findings);
}

#[test]
fn fold_equal_values_with_opposite_ops_at_the_same_tick_are_not_a_conflict() {
    let (mut w, a, _, _) = world();
    w.assert(a, "name", s("Edwin"), t(0)).unwrap();
    // different values (different bytes): assert EDWIN and retract Edwin at tick 10
    w.assert(a, "name", s("EDWIN"), t(10)).unwrap();
    let r = w.retract(a, "name", s("Edwin"), t(10)).unwrap();
    assert_eq!(r.changed, 1);
    // the entity stays found through the swap, by every spelling
    for at in [0, 9, 10, 11] {
        assert_eq!(ci(&w, "name", "edwin", at), vec![a], "@{at}");
    }
    assert_eq!(w.lookup("name", &s("Edwin"), Tick(10)).unwrap(), vec![]);
    assert_eq!(w.lookup("name", &s("EDWIN"), Tick(10)).unwrap(), vec![a]);
    // and each value's own histories are untouched: retract the new one too
    w.retract(a, "name", s("EDWIN"), t(20)).unwrap();
    assert_eq!(ci(&w, "name", "edwin", 19), vec![a]);
    assert_eq!(ci(&w, "name", "edwin", 20), vec![]);
    clean(&w);
}

#[test]
fn the_same_value_with_opposite_ops_at_one_tick_still_conflicts_through_the_fold() {
    let (mut w, a, _, _) = world();
    w.assert(a, "name", s("Edwin"), t(5)).unwrap();
    assert!(matches!(
        w.retract(a, "name", s("Edwin"), t(5)).unwrap_err(),
        Error::SameTickConflict { .. }
    ));
    assert_eq!(ci(&w, "name", "EDWIN", 5), vec![a]);
    clean(&w);
}

#[test]
fn an_edge_property_is_still_found_after_its_edge_triple_is_retracted() {
    let (mut w, a, _, p) = world();
    w.assert(a, "present", Value::Ref(p), t(0)).unwrap();
    let edge = EntityId::from(w.edge_id(a, "present", p).unwrap());
    w.assert(edge, "note", s("In Disguise"), t(0)).unwrap();
    w.retract(a, "present", Value::Ref(p), t(10)).unwrap();
    for at in [0, 9, 10, 11, 1_000] {
        assert_eq!(ci(&w, "note", "in DISGUISE", at), vec![edge], "@{at}");
    }
    // the property ends only by its own retract
    w.retract(edge, "note", s("In Disguise"), t(20)).unwrap();
    assert_eq!(ci(&w, "note", "in disguise", 19), vec![edge]);
    assert_eq!(ci(&w, "note", "in disguise", 20), vec![]);
    clean(&w);
}

#[test]
fn folds_that_are_byte_prefixes_of_each_other_never_over_match() {
    let (mut w, a, b, p) = world();
    w.assert(a, "name", s("Ed"), t(0)).unwrap();
    w.assert(b, "name", s("EDWIN"), t(0)).unwrap();
    let c = p; // a third holder: use a PLACE as a name carrier
    w.assert(c, "name", s("Edwina"), t(0)).unwrap();
    assert_eq!(ci(&w, "name", "ed", 0), vec![a]);
    assert_eq!(ci(&w, "name", "edwin", 0), vec![b]);
    assert_eq!(ci(&w, "name", "edwina", 0), vec![c]);
    assert_eq!(ci(&w, "name", "edw", 0), vec![]);
    assert_eq!(ci(&w, "name", "edwinab", 0), vec![]);
    assert_eq!(ci(&w, "name", "e", 0), vec![]);
    clean(&w);
}

#[test]
fn folds_containing_nul_never_over_match() {
    let (mut w, a, b, p) = world();
    w.assert(a, "name", s("a"), t(0)).unwrap();
    w.assert(b, "name", s("a\0"), t(0)).unwrap();
    w.assert(p, "name", s("A\0b"), t(0)).unwrap();
    assert_eq!(ci(&w, "name", "A", 0), vec![a]);
    assert_eq!(ci(&w, "name", "a\0", 0), vec![b]);
    assert_eq!(ci(&w, "name", "a\0B", 0), vec![p]);
    assert_eq!(ci(&w, "name", "a\0\0", 0), vec![]);
    assert_eq!(ci(&w, "name", "\0", 0), vec![]);
    assert_eq!(ci(&w, "name", "a\0b\0", 0), vec![]);
    // exact lookups agree on the same distinction
    assert_eq!(w.lookup("name", &s("a"), Tick(0)).unwrap(), vec![a]);
    assert_eq!(w.lookup("name", &s("a\0"), Tick(0)).unwrap(), vec![b]);
    clean(&w);
}

#[test]
fn a_nul_only_and_an_empty_value_are_distinct_folds() {
    let (mut w, a, b, _) = world();
    w.assert(a, "name", s(""), t(0)).unwrap();
    w.assert(b, "name", s("\0"), t(0)).unwrap();
    assert_eq!(ci(&w, "name", "", 0), vec![a]);
    assert_eq!(ci(&w, "name", "\0", 0), vec![b]);
    clean(&w);
}

#[test]
fn the_fold_of_the_empty_string_is_empty_and_only_it_finds_it() {
    assert_eq!(fold_for_search(""), "");
    let (mut w, a, b, _) = world();
    w.assert(a, "name", s(""), t(0)).unwrap();
    w.assert(b, "name", s("x"), t(0)).unwrap();
    assert_eq!(ci(&w, "name", "", 0), vec![a]);
    assert_eq!(ci(&w, "name", "x", 0), vec![b]);
    assert_eq!(ci(&w, "name", "", -1), vec![]);
    // an empty query never matches non-empty values
    w.retract(a, "name", s(""), t(3)).unwrap();
    assert_eq!(ci(&w, "name", "", 3), vec![]);
    clean(&w);
}
