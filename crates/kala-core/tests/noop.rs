//! Item 7: no-op rules and `changed`. Explicit ticks only. A write that
//! changes nothing stores nothing and does not advance latest_tx.
//! Never write assert and retract of the same (e,a,v) at the same tick
//! (item 8), and no tick defaulting (9), event retracts (10) or txns (11).

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
}

struct Fx {
    w: World,
    greg: EntityId,
    varrow: EntityId,
    ashford: EntityId,
    carnath: EntityId,
}

fn fx() -> Fx {
    let mut w = World::in_memory(schema()).expect("world");
    let greg = w.create_entity("CHARACTER", Some("greg")).unwrap();
    let varrow = w.create_entity("PLACE", Some("varrow")).unwrap();
    let ashford = w.create_entity("PLACE", Some("ashford")).unwrap();
    let carnath = w.create_entity("PLACE", Some("carnath")).unwrap();
    Fx {
        w,
        greg,
        varrow,
        ashford,
        carnath,
    }
}

fn at(t: i64) -> Option<Tick> {
    Some(Tick(t))
}

fn holds(w: &World, e: EntityId, a: &str, v: Value, t: i64) -> bool {
    w.as_of_value(e, a, &v, Tick(t)).unwrap()
}

fn place(f: &Fx, name: &str) -> EntityId {
    match name {
        "varrow" => f.varrow,
        "ashford" => f.ashford,
        _ => f.carnath,
    }
}

fn put(f: &mut Fx, p: &str, t: i64) -> WriteResult {
    let p = place(f, p);
    f.w.assert(f.greg, "located_in", Value::Ref(p), at(t))
        .unwrap()
}

fn del(f: &mut Fx, p: &str, t: i64) -> WriteResult {
    let p = place(f, p);
    f.w.retract(f.greg, "located_in", Value::Ref(p), at(t))
        .unwrap()
}

fn loc(f: &Fx, place: EntityId, t: i64) -> bool {
    holds(&f.w, f.greg, "located_in", Value::Ref(place), t)
}

#[test]
fn first_assert_changes_one_and_reports_tick() {
    let mut f = fx();
    let before = f.w.latest_tx();
    let r = put(&mut f, "varrow", -300);
    assert_eq!(r.changed, 1);
    assert_eq!(r.tick, Tick(-300));
    assert_eq!(f.w.latest_tx(), TxId(before.0 + 1));
}

#[test]
fn assert_of_value_already_holding_same_tick_is_noop() {
    let mut f = fx();
    put(&mut f, "varrow", -300);
    let tx = f.w.latest_tx();
    let r = put(&mut f, "varrow", -300);
    assert_eq!(r.changed, 0);
    assert_eq!(r.tick, Tick(-300));
    assert_eq!(f.w.latest_tx(), tx, "0-changed write must not advance tx");
    assert!(loc(&f, f.varrow, -300));
    assert_eq!(
        f.w.as_of(f.greg, "located_in", Tick(-300)).unwrap().len(),
        1
    );
}

#[test]
fn assert_at_later_tick_while_holding_is_noop() {
    let mut f = fx();
    put(&mut f, "varrow", -300);
    let tx = f.w.latest_tx();
    let r = put(&mut f, "varrow", -100);
    assert_eq!(r.changed, 0);
    assert_eq!(r.tick, Tick(-100));
    assert_eq!(f.w.latest_tx(), tx);
    // nothing stored at -100: a retract at -150 ends the value, and -100 stays absent
    assert_eq!(del(&mut f, "varrow", -150).changed, 1);
    assert!(!loc(&f, f.varrow, -100));
}

#[test]
fn assert_at_earlier_tick_prequel_while_holding_later_changes_one() {
    let mut f = fx();
    put(&mut f, "varrow", -300);
    let tx = f.w.latest_tx();
    let r = put(&mut f, "varrow", -400);
    assert_eq!(r.changed, 1, "doesn't hold at -400, so it is stored");
    assert_eq!(r.tick, Tick(-400));
    assert_eq!(f.w.latest_tx(), TxId(tx.0 + 1));
    assert!(!loc(&f, f.varrow, -401));
    assert!(loc(&f, f.varrow, -400));
    assert!(loc(&f, f.varrow, -350));
}

#[test]
fn retract_of_never_asserted_value_is_noop() {
    let mut f = fx();
    let tx = f.w.latest_tx();
    let r = del(&mut f, "varrow", 0);
    assert_eq!(r.changed, 0);
    assert_eq!(r.tick, Tick(0));
    assert_eq!(f.w.latest_tx(), tx);
    assert!(!loc(&f, f.varrow, 0));
    assert!(!loc(&f, f.varrow, i64::MAX));
}

#[test]
fn retract_before_the_assert_tick_is_noop() {
    let mut f = fx();
    put(&mut f, "varrow", -300);
    let tx = f.w.latest_tx();
    let r = del(&mut f, "varrow", -400);
    assert_eq!(r.changed, 0);
    assert_eq!(r.tick, Tick(-400));
    assert_eq!(f.w.latest_tx(), tx);
    // the stored assert is unaffected
    assert!(loc(&f, f.varrow, -300));
    assert!(loc(&f, f.varrow, 0));
}

#[test]
fn retract_after_already_retracted_is_noop() {
    let mut f = fx();
    put(&mut f, "varrow", -300);
    assert_eq!(del(&mut f, "varrow", -250).changed, 1);
    let tx = f.w.latest_tx();
    let r = del(&mut f, "varrow", -100);
    assert_eq!(r.changed, 0);
    assert_eq!(r.tick, Tick(-100));
    assert_eq!(f.w.latest_tx(), tx);
    // repeating the very same retract is also a no-op
    assert_eq!(del(&mut f, "varrow", -250).changed, 0);
    assert_eq!(f.w.latest_tx(), tx);
}

#[test]
fn retract_while_holding_changes_one() {
    let mut f = fx();
    put(&mut f, "varrow", -300);
    let tx = f.w.latest_tx();
    let r = del(&mut f, "varrow", -250);
    assert_eq!(r.changed, 1);
    assert_eq!(r.tick, Tick(-250));
    assert_eq!(f.w.latest_tx(), TxId(tx.0 + 1));
    assert!(loc(&f, f.varrow, -251));
    assert!(!loc(&f, f.varrow, -250));
}

#[test]
fn out_of_order_retract_inside_a_closed_interval_is_stored() {
    // holds [-300, -250); retract @-280 is judged at -280 (he was there)
    let mut f = fx();
    assert_eq!(put(&mut f, "varrow", -300).changed, 1);
    assert_eq!(del(&mut f, "varrow", -250).changed, 1);
    let tx = f.w.latest_tx();
    let r = del(&mut f, "varrow", -280);
    assert_eq!(r.changed, 1);
    assert_eq!(r.tick, Tick(-280));
    assert_eq!(f.w.latest_tx(), TxId(tx.0 + 1));
    assert!(!loc(&f, f.varrow, -280));
    assert!(loc(&f, f.varrow, -290));
    assert!(!loc(&f, f.varrow, -260));
    assert!(!loc(&f, f.varrow, -250));
    assert!(loc(&f, f.varrow, -281));
}

#[test]
fn assert_after_a_retract_changes_one() {
    let mut f = fx();
    put(&mut f, "varrow", -300);
    del(&mut f, "varrow", -250);
    let tx = f.w.latest_tx();
    let r = put(&mut f, "varrow", -200);
    assert_eq!(r.changed, 1);
    assert_eq!(r.tick, Tick(-200));
    assert_eq!(f.w.latest_tx(), TxId(tx.0 + 1));
    assert!(loc(&f, f.varrow, -200));
    assert!(!loc(&f, f.varrow, -220));
}

#[test]
fn assert_in_the_gap_between_assert_and_retract_is_noop_but_after_retract_is_not() {
    let mut f = fx();
    put(&mut f, "varrow", -300);
    del(&mut f, "varrow", -250);
    // at -275 it still holds
    assert_eq!(put(&mut f, "varrow", -275).changed, 0);
    // at -250 exactly it was just retracted (assert here would be a same-tick
    // conflict, item 8), so use -249
    assert_eq!(put(&mut f, "varrow", -249).changed, 1);
}

#[test]
fn noop_is_judged_per_value_not_per_attribute() {
    let mut f = fx();
    put(&mut f, "varrow", 0);
    // another value of a cardinality-one attribute is a separate history
    assert_eq!(put(&mut f, "ashford", 0).changed, 1);
    assert_eq!(del(&mut f, "carnath", 0).changed, 0);
}

#[test]
fn noop_is_judged_per_entity() {
    let mut f = fx();
    let mara = f.w.create_entity("CHARACTER", Some("mara")).unwrap();
    put(&mut f, "varrow", 0);
    let r =
        f.w.assert(mara, "located_in", Value::Ref(f.varrow), at(0))
            .unwrap();
    assert_eq!(r.changed, 1);
}

#[test]
fn noop_writes_still_validate_input() {
    let mut f = fx();
    put(&mut f, "varrow", 0);
    assert!(matches!(
        f.w.assert(f.greg, "located_in", Value::Bool(true), at(0)),
        Err(Error::WrongValueType { .. })
    ));
    assert_eq!(
        f.w.retract(f.greg, "nope", Value::Bool(true), at(0)),
        Err(Error::UnknownAttribute("nope".to_string()))
    );
}

#[test]
fn bool_value_noop_rules() {
    let mut f = fx();
    let r =
        f.w.assert(f.greg, "alive", Value::Bool(true), at(10))
            .unwrap();
    assert_eq!(r.changed, 1);
    let r =
        f.w.assert(f.greg, "alive", Value::Bool(true), at(20))
            .unwrap();
    assert_eq!(r.changed, 0);
    // true and false are different values: false isn't holding
    let r =
        f.w.retract(f.greg, "alive", Value::Bool(false), at(20))
            .unwrap();
    assert_eq!(r.changed, 0);
    let r =
        f.w.assert(f.greg, "alive", Value::Bool(false), at(20))
            .unwrap();
    assert_eq!(r.changed, 1);
}

#[test]
fn repeated_retract_at_same_tick_as_stored_retract_is_noop() {
    let mut f = fx();
    put(&mut f, "varrow", 10);
    assert_eq!(del(&mut f, "varrow", 20).changed, 1);
    let tx = f.w.latest_tx();
    let r = del(&mut f, "varrow", 20);
    assert_eq!(r.changed, 0);
    assert_eq!(r.tick, Tick(20));
    assert_eq!(f.w.latest_tx(), tx);
    assert!(loc(&f, f.varrow, 19));
    assert!(!loc(&f, f.varrow, 20));
}

#[test]
fn noop_judged_at_i64_min() {
    let mut f = fx();
    let tx = f.w.latest_tx();
    // nothing holds: retract is a no-op
    assert_eq!(del(&mut f, "varrow", i64::MIN).changed, 0);
    assert_eq!(f.w.latest_tx(), tx);
    assert_eq!(put(&mut f, "varrow", i64::MIN).changed, 1);
    let tx = f.w.latest_tx();
    let r = put(&mut f, "varrow", i64::MIN);
    assert_eq!(r.changed, 0);
    assert_eq!(r.tick, Tick(i64::MIN));
    assert_eq!(f.w.latest_tx(), tx);
    assert!(loc(&f, f.varrow, i64::MIN));
}

#[test]
fn noop_judged_at_zero() {
    let mut f = fx();
    assert_eq!(del(&mut f, "varrow", 0).changed, 0);
    assert_eq!(put(&mut f, "varrow", 0).changed, 1);
    let tx = f.w.latest_tx();
    assert_eq!(put(&mut f, "varrow", 0).changed, 0);
    assert_eq!(f.w.latest_tx(), tx);
    assert!(!loc(&f, f.varrow, -1));
    assert!(loc(&f, f.varrow, 0));
}

#[test]
fn noop_judged_at_i64_max() {
    let mut f = fx();
    assert_eq!(del(&mut f, "varrow", i64::MAX).changed, 0);
    assert_eq!(put(&mut f, "varrow", i64::MAX).changed, 1);
    let tx = f.w.latest_tx();
    let r = put(&mut f, "varrow", i64::MAX);
    assert_eq!(r.changed, 0);
    assert_eq!(r.tick, Tick(i64::MAX));
    assert_eq!(f.w.latest_tx(), tx);
    assert!(!loc(&f, f.varrow, i64::MAX - 1));
    assert!(loc(&f, f.varrow, i64::MAX));
}

#[test]
fn assert_at_min_then_assert_at_max_is_noop() {
    let mut f = fx();
    assert_eq!(put(&mut f, "varrow", i64::MIN).changed, 1);
    let tx = f.w.latest_tx();
    let r = put(&mut f, "varrow", i64::MAX);
    assert_eq!(r.changed, 0);
    assert_eq!(r.tick, Tick(i64::MAX));
    assert_eq!(f.w.latest_tx(), tx);
}

#[test]
fn retract_at_max_after_assert_at_min_is_stored() {
    let mut f = fx();
    put(&mut f, "varrow", i64::MIN);
    assert_eq!(del(&mut f, "varrow", i64::MAX).changed, 1);
    assert!(loc(&f, f.varrow, i64::MAX - 1));
    assert!(!loc(&f, f.varrow, i64::MAX));
}

#[test]
fn assert_between_retract_and_next_assert_is_judged_at_its_own_tick() {
    // A@10, R@20, A@30: holds [10,20) and [30,inf)
    let mut f = fx();
    assert_eq!(put(&mut f, "varrow", 10).changed, 1);
    assert_eq!(del(&mut f, "varrow", 20).changed, 1);
    assert_eq!(put(&mut f, "varrow", 30).changed, 1);
    // not holding at 25: stored
    let tx = f.w.latest_tx();
    let r = put(&mut f, "varrow", 25);
    assert_eq!(r.changed, 1);
    assert_eq!(f.w.latest_tx(), TxId(tx.0 + 1));
    // now holds at 27 (assert @25 is the latest op <= 27): no-op
    let tx = f.w.latest_tx();
    let r = put(&mut f, "varrow", 27);
    assert_eq!(r.changed, 0);
    assert_eq!(f.w.latest_tx(), tx);
    assert!(!loc(&f, f.varrow, 24));
    assert!(loc(&f, f.varrow, 25));
    assert!(loc(&f, f.varrow, 27));
    assert!(loc(&f, f.varrow, 100));
}

#[test]
fn assert_negative_zero_while_positive_zero_holds_is_noop() {
    let mut f = fx();
    let r =
        f.w.assert(f.greg, "weight", Value::F64(0.0), at(10))
            .unwrap();
    assert_eq!(r.changed, 1);
    let tx = f.w.latest_tx();
    let r =
        f.w.assert(f.greg, "weight", Value::F64(-0.0), at(10))
            .unwrap();
    assert_eq!(r.changed, 0);
    assert_eq!(f.w.latest_tx(), tx);
    let r =
        f.w.assert(f.greg, "weight", Value::F64(-0.0), at(20))
            .unwrap();
    assert_eq!(r.changed, 0);
    assert_eq!(f.w.as_of(f.greg, "weight", Tick(15)).unwrap().len(), 1);
}

#[test]
fn retract_negative_zero_ends_positive_zero() {
    let mut f = fx();
    f.w.assert(f.greg, "weight", Value::F64(0.0), at(10))
        .unwrap();
    let r =
        f.w.retract(f.greg, "weight", Value::F64(-0.0), at(20))
            .unwrap();
    assert_eq!(r.changed, 1);
    assert!(holds(&f.w, f.greg, "weight", Value::F64(0.0), 19));
    assert!(!holds(&f.w, f.greg, "weight", Value::F64(0.0), 20));
}

#[test]
fn ref_value_noop_rules() {
    let mut f = fx();
    let tx = f.w.latest_tx();
    let r =
        f.w.retract(f.greg, "owns", Value::Ref(f.varrow), at(5))
            .unwrap();
    assert_eq!(r.changed, 0);
    assert_eq!(f.w.latest_tx(), tx);
    let r =
        f.w.assert(f.greg, "owns", Value::Ref(f.varrow), at(5))
            .unwrap();
    assert_eq!(r.changed, 1);
    let tx = f.w.latest_tx();
    let r =
        f.w.assert(f.greg, "owns", Value::Ref(f.varrow), at(8))
            .unwrap();
    assert_eq!(r.changed, 0);
    assert_eq!(f.w.latest_tx(), tx);
    // a different ref is a different value
    let r =
        f.w.assert(f.greg, "owns", Value::Ref(f.ashford), at(8))
            .unwrap();
    assert_eq!(r.changed, 1);
    let r =
        f.w.retract(f.greg, "owns", Value::Ref(f.carnath), at(9))
            .unwrap();
    assert_eq!(r.changed, 0);
}
