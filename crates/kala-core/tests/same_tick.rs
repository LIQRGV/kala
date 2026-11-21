//! Item 8: same-tick conflicts. Opposite ops for the same (entity, attribute,
//! value) at the same tick are an error, even in a later transaction; the
//! same op repeated is a no-op. Explicit ticks, auto-commit writes only.

use kala_core::*;

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
            "owns",
            ValueType::Ref,
            Kind::State,
            Cardinality::Many,
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
            Kind::State,
            Cardinality::Many,
        ))
        .attribute(AttrDef::new(
            "weight",
            ValueType::F64,
            Kind::State,
            Cardinality::One,
        ))
}

struct Fx {
    w: World,
    greg: EntityId,
    mara: EntityId,
    varrow: EntityId,
    ashford: EntityId,
}

fn fx() -> Fx {
    let mut w = World::in_memory(schema()).expect("world");
    let greg = w.create_entity("CHARACTER", Some("greg")).unwrap();
    let mara = w.create_entity("CHARACTER", Some("mara")).unwrap();
    let varrow = w.create_entity("PLACE", Some("varrow")).unwrap();
    let ashford = w.create_entity("PLACE", Some("ashford")).unwrap();
    Fx {
        w,
        greg,
        mara,
        varrow,
        ashford,
    }
}

fn t(x: i64) -> Option<Tick> {
    Some(Tick(x))
}

fn put(f: &mut Fx, place: EntityId, tick: i64) -> Result<WriteResult, Error> {
    f.w.assert(f.greg, "located_in", Value::Ref(place), t(tick))
}

fn del(f: &mut Fx, place: EntityId, tick: i64) -> Result<WriteResult, Error> {
    f.w.retract(f.greg, "located_in", Value::Ref(place), t(tick))
}

fn here(f: &Fx, place: EntityId, tick: i64) -> bool {
    f.w.as_of_value(f.greg, "located_in", &Value::Ref(place), Tick(tick))
        .unwrap()
}

fn conflict(f: &Fx, tick: i64) -> Error {
    Error::SameTickConflict {
        entity: f.greg,
        attr: "located_in".to_string(),
        tick: Tick(tick),
    }
}

#[test]
fn retract_at_the_tick_of_an_assert_is_a_conflict() {
    let mut f = fx();
    let v = f.varrow;
    put(&mut f, v, 10).unwrap();
    assert_eq!(del(&mut f, v, 10), Err(conflict(&f, 10)));
}

#[test]
fn assert_at_the_tick_of_a_stored_retract_is_a_conflict() {
    let mut f = fx();
    let v = f.varrow;
    put(&mut f, v, 0).unwrap();
    assert_eq!(del(&mut f, v, 10).unwrap().changed, 1);
    assert_eq!(put(&mut f, v, 10), Err(conflict(&f, 10)));
}

#[test]
fn conflict_is_detected_in_a_later_transaction() {
    let mut f = fx();
    let (v, a) = (f.varrow, f.ashford);
    put(&mut f, v, 10).unwrap();
    // several unrelated txs in between
    put(&mut f, a, 50).unwrap();
    f.w.assert(f.greg, "alive", Value::Bool(true), t(1))
        .unwrap();
    f.w.assert(f.mara, "located_in", Value::Ref(v), t(10))
        .unwrap();
    f.w.create_entity("PLACE", Some("carnath")).unwrap();
    del(&mut f, a, 60).unwrap();
    assert_eq!(del(&mut f, v, 10), Err(conflict(&f, 10)));
}

#[test]
fn conflict_error_stores_nothing() {
    let mut f = fx();
    let v = f.varrow;
    put(&mut f, v, 10).unwrap();
    let tx = f.w.latest_tx();
    assert!(del(&mut f, v, 10).is_err());
    assert_eq!(f.w.latest_tx(), tx);
    assert!(!here(&f, v, 9));
    assert!(here(&f, v, 10));
    assert!(here(&f, v, 11));
    assert_eq!(f.w.as_of(f.greg, "located_in", Tick(10)).unwrap().len(), 1);

    // the other direction: stored retract, conflicting assert
    let mut f = fx();
    put(&mut f, v, 0).unwrap();
    del(&mut f, v, 10).unwrap();
    let tx = f.w.latest_tx();
    assert!(put(&mut f, v, 10).is_err());
    assert_eq!(f.w.latest_tx(), tx);
    assert!(here(&f, v, 9));
    assert!(!here(&f, v, 10));
    assert!(!here(&f, v, 11));
    assert!(
        f.w.as_of(f.greg, "located_in", Tick(10))
            .unwrap()
            .is_empty()
    );
}

#[test]
fn repeated_assert_at_same_tick_is_noop() {
    let mut f = fx();
    let v = f.varrow;
    assert_eq!(put(&mut f, v, 10).unwrap().changed, 1);
    let tx = f.w.latest_tx();
    let r = put(&mut f, v, 10).unwrap();
    assert_eq!(r.changed, 0);
    assert_eq!(r.tick, Tick(10));
    assert_eq!(f.w.latest_tx(), tx);
}

#[test]
fn repeated_retract_at_same_tick_is_noop() {
    let mut f = fx();
    let v = f.varrow;
    put(&mut f, v, 10).unwrap();
    assert_eq!(del(&mut f, v, 20).unwrap().changed, 1);
    let tx = f.w.latest_tx();
    let r = del(&mut f, v, 20).unwrap();
    assert_eq!(r.changed, 0);
    assert_eq!(f.w.latest_tx(), tx);
}

#[test]
fn different_values_at_the_same_tick_are_not_a_conflict() {
    let mut f = fx();
    let (v, a) = (f.varrow, f.ashford);
    put(&mut f, v, 200).unwrap();
    assert_eq!(del(&mut f, v, 250).unwrap().changed, 1);
    assert_eq!(put(&mut f, a, 250).unwrap().changed, 1);
    assert!(here(&f, v, 249));
    assert!(!here(&f, v, 250));
    assert!(!here(&f, a, 249));
    assert!(here(&f, a, 250));
}

#[test]
fn different_entities_same_value_and_tick_are_not_a_conflict() {
    let mut f = fx();
    let v = f.varrow;
    put(&mut f, v, 0).unwrap();
    f.w.assert(f.mara, "located_in", Value::Ref(v), t(0))
        .unwrap();
    // Greg retracts at 10; Mara asserts (already holds) at 10: no conflict
    assert_eq!(del(&mut f, v, 10).unwrap().changed, 1);
    let r =
        f.w.assert(f.mara, "located_in", Value::Ref(v), t(10))
            .unwrap();
    assert_eq!(r.changed, 0);
    // Mara retracts at 10 where Greg's retract is stored: separate histories
    let r =
        f.w.retract(f.mara, "located_in", Value::Ref(v), t(10))
            .unwrap();
    assert_eq!(r.changed, 1);
}

#[test]
fn different_attributes_same_value_and_tick_are_not_a_conflict() {
    let mut f = fx();
    let v = f.varrow;
    put(&mut f, v, 10).unwrap();
    f.w.assert(f.greg, "owns", Value::Ref(v), t(0)).unwrap();
    // retract owns @10 where located_in has an assert @10 for the same value
    let r = f.w.retract(f.greg, "owns", Value::Ref(v), t(10)).unwrap();
    assert_eq!(r.changed, 1);
    assert!(here(&f, v, 10));
}

#[test]
fn conflict_at_i64_min() {
    let mut f = fx();
    let v = f.varrow;
    put(&mut f, v, i64::MIN).unwrap();
    assert_eq!(del(&mut f, v, i64::MIN), Err(conflict(&f, i64::MIN)));
    assert!(here(&f, v, i64::MIN));
}

#[test]
fn conflict_at_i64_max() {
    let mut f = fx();
    let v = f.varrow;
    put(&mut f, v, 0).unwrap();
    assert_eq!(del(&mut f, v, i64::MAX).unwrap().changed, 1);
    assert_eq!(put(&mut f, v, i64::MAX), Err(conflict(&f, i64::MAX)));
    assert!(!here(&f, v, i64::MAX));
}

#[test]
fn adjacent_ticks_are_not_a_conflict() {
    let mut f = fx();
    let v = f.varrow;
    put(&mut f, v, 10).unwrap();
    assert_eq!(del(&mut f, v, 11).unwrap().changed, 1);
    assert!(here(&f, v, 10));
    assert!(!here(&f, v, 11));
}

#[test]
fn conflict_detected_when_stored_op_is_not_the_latest_overall() {
    // assert @10, retract @20, then retract @10: a retract @10 against the
    // assert @10 is a conflict (judged on the same-tick entry, not on state)
    let mut f = fx();
    let v = f.varrow;
    put(&mut f, v, 10).unwrap();
    del(&mut f, v, 20).unwrap();
    let tx = f.w.latest_tx();
    assert_eq!(del(&mut f, v, 10), Err(conflict(&f, 10)));
    assert_eq!(f.w.latest_tx(), tx);
}

#[test]
fn conflict_detected_with_later_assert_after_a_later_retract() {
    let mut f = fx();
    let v = f.varrow;
    put(&mut f, v, 10).unwrap();
    del(&mut f, v, 20).unwrap();
    put(&mut f, v, 30).unwrap();
    let tx = f.w.latest_tx();
    assert_eq!(del(&mut f, v, 10), Err(conflict(&f, 10)));
    assert_eq!(f.w.latest_tx(), tx);
    // and the mirror: assert at the tick of the stored retract @20
    assert_eq!(put(&mut f, v, 20), Err(conflict(&f, 20)));
    assert_eq!(f.w.latest_tx(), tx);
}

#[test]
fn conflict_even_when_the_value_does_not_hold_at_that_tick() {
    // assert @0, retract @10, then assert @10: the value is absent at 10, but
    // the stored retract @10 is the opposite op at the same tick.
    let mut f = fx();
    let v = f.varrow;
    put(&mut f, v, 0).unwrap();
    del(&mut f, v, 10).unwrap();
    assert!(!here(&f, v, 10));
    let tx = f.w.latest_tx();
    assert_eq!(put(&mut f, v, 10), Err(conflict(&f, 10)));
    assert_eq!(f.w.latest_tx(), tx);
}

#[test]
fn conflict_at_tick_zero() {
    let mut f = fx();
    let v = f.varrow;
    put(&mut f, v, 0).unwrap();
    assert_eq!(del(&mut f, v, 0), Err(conflict(&f, 0)));
    assert!(here(&f, v, 0));
}

#[test]
fn minus_one_versus_zero_is_not_a_conflict() {
    let mut f = fx();
    let v = f.varrow;
    put(&mut f, v, -1).unwrap();
    assert_eq!(del(&mut f, v, 0).unwrap().changed, 1);
    assert!(here(&f, v, -1));
    assert!(!here(&f, v, 0));
}

#[test]
fn i64_min_versus_next_tick_is_not_a_conflict() {
    let mut f = fx();
    let v = f.varrow;
    put(&mut f, v, i64::MIN).unwrap();
    assert_eq!(del(&mut f, v, i64::MIN + 1).unwrap().changed, 1);
    assert!(here(&f, v, i64::MIN));
    assert!(!here(&f, v, i64::MIN + 1));
}

#[test]
fn string_values_that_are_prefixes_of_each_other_do_not_conflict() {
    let mut f = fx();
    let s = |x: &str| Value::Str(x.to_string());
    f.w.assert(f.greg, "title", s("Var"), t(5)).unwrap();
    // "Varrow" is not asserted: retract is a no-op, not a conflict with "Var"
    let r = f.w.retract(f.greg, "title", s("Varrow"), t(5)).unwrap();
    assert_eq!(r.changed, 0);
    // assert "Varrow" @5 then retract "Var" @5 would be the same-op-different-value case
    assert_eq!(
        f.w.assert(f.greg, "title", s("Varrow"), t(5))
            .unwrap()
            .changed,
        1
    );
    f.w.assert(f.greg, "title", s("Var"), t(1)).unwrap();
    // opposite ops at the same tick on "Var" (retract @6) and "Varrow" (assert @6)
    assert_eq!(
        f.w.retract(f.greg, "title", s("Var"), t(6))
            .unwrap()
            .changed,
        1
    );
    assert_eq!(
        f.w.assert(f.greg, "title", s("Varrow"), t(6))
            .unwrap()
            .changed,
        0 // already holds at 6
    );
    let held =
        |w: &World, x: &str, tick| w.as_of_value(f.greg, "title", &s(x), Tick(tick)).unwrap();
    assert!(held(&f.w, "Varrow", 6));
    assert!(!held(&f.w, "Var", 6));
    // same tick, opposite ops, different strings, both stored
    assert_eq!(
        f.w.retract(f.greg, "title", s("Varrow"), t(7))
            .unwrap()
            .changed,
        1
    );
    assert_eq!(
        f.w.assert(f.greg, "title", s("Var"), t(7)).unwrap().changed,
        1
    );
    assert!(!held(&f.w, "Varrow", 7));
    assert!(held(&f.w, "Var", 7));
}

#[test]
fn negative_zero_and_zero_are_the_same_value_for_conflicts() {
    let mut f = fx();
    f.w.assert(f.greg, "weight", Value::F64(0.0), t(3)).unwrap();
    let r = f.w.retract(f.greg, "weight", Value::F64(-0.0), t(3));
    assert_eq!(
        r,
        Err(Error::SameTickConflict {
            entity: f.greg,
            attr: "weight".to_string(),
            tick: Tick(3),
        })
    );
    assert!(
        f.w.as_of_value(f.greg, "weight", &Value::F64(0.0), Tick(3))
            .unwrap()
    );
}
