//! Item 6: retract. Explicit ticks only; no changed counts (7), same-tick
//! conflicts (8), tick defaults (9), event retracts (10), transactions (11).
//! Never write assert and retract of the same (e,a,v) at the same tick.

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

fn loc(f: &Fx, place: EntityId, t: i64) -> bool {
    holds(&f.w, f.greg, "located_in", Value::Ref(place), t)
}

#[test]
fn retracted_value_absent_from_retract_tick_on_present_before() {
    let mut f = fx();
    f.w.assert(f.greg, "located_in", Value::Ref(f.varrow), at(-300))
        .unwrap();
    f.w.retract(f.greg, "located_in", Value::Ref(f.varrow), at(-250))
        .unwrap();
    assert!(!loc(&f, f.varrow, -301));
    assert!(loc(&f, f.varrow, -300));
    assert!(loc(&f, f.varrow, -251));
    assert!(!loc(&f, f.varrow, -250));
    assert!(!loc(&f, f.varrow, -220));
    assert!(!loc(&f, f.varrow, 0));
    assert!(!loc(&f, f.varrow, i64::MAX));
    assert_eq!(
        f.w.as_of(f.greg, "located_in", Tick(-251)).unwrap(),
        vec![Value::Ref(f.varrow)]
    );
    assert_eq!(f.w.as_of(f.greg, "located_in", Tick(-250)).unwrap(), vec![]);
}

#[test]
fn retract_at_extreme_ticks() {
    let mut f = fx();
    f.w.assert(f.greg, "alive", Value::Bool(true), at(i64::MIN))
        .unwrap();
    f.w.retract(f.greg, "alive", Value::Bool(true), at(i64::MAX))
        .unwrap();
    assert!(holds(
        &f.w,
        f.greg,
        "alive",
        Value::Bool(true),
        i64::MAX - 1
    ));
    assert!(!holds(&f.w, f.greg, "alive", Value::Bool(true), i64::MAX));
    assert!(holds(&f.w, f.greg, "alive", Value::Bool(true), i64::MIN));
}

#[test]
fn assert_never_ends_another_value_overlap_visible_until_retracted() {
    let mut f = fx();
    f.w.assert(f.greg, "located_in", Value::Ref(f.varrow), at(200))
        .unwrap();
    f.w.assert(f.greg, "located_in", Value::Ref(f.ashford), at(601))
        .unwrap();
    // cardinality one is a label: both hold from 601, nothing was ended
    assert!(loc(&f, f.varrow, 700));
    assert!(loc(&f, f.ashford, 700));
    assert_eq!(f.w.as_of(f.greg, "located_in", Tick(700)).unwrap().len(), 2);
    // the author later decides when he left Varrow (tx#57)
    f.w.retract(f.greg, "located_in", Value::Ref(f.varrow), at(250))
        .unwrap();
    assert!(loc(&f, f.varrow, 240));
    assert!(!loc(&f, f.ashford, 240));
    assert!(!loc(&f, f.varrow, 300));
    assert!(!loc(&f, f.ashford, 300));
    assert_eq!(f.w.as_of(f.greg, "located_in", Tick(300)).unwrap(), vec![]);
    assert!(!loc(&f, f.varrow, 700));
    assert!(loc(&f, f.ashford, 700));
    assert_eq!(
        f.w.as_of(f.greg, "located_in", Tick(700)).unwrap(),
        vec![Value::Ref(f.ashford)]
    );
}

#[test]
fn assert_later_does_not_end_earlier_value_with_no_retract() {
    let mut f = fx();
    f.w.assert(f.greg, "located_in", Value::Ref(f.varrow), at(-300))
        .unwrap();
    f.w.assert(f.greg, "located_in", Value::Ref(f.ashford), at(-250))
        .unwrap();
    assert!(loc(&f, f.varrow, -100));
    assert!(loc(&f, f.ashford, -100));
}

#[test]
fn re_assert_after_retract_claude_md_example() {
    // R|Varrow|located_in|Greg: assert -300, retract -250, assert -200
    let mut f = fx();
    f.w.assert(f.greg, "located_in", Value::Ref(f.varrow), at(-300))
        .unwrap();
    f.w.retract(f.greg, "located_in", Value::Ref(f.varrow), at(-250))
        .unwrap();
    f.w.assert(f.greg, "located_in", Value::Ref(f.varrow), at(-200))
        .unwrap();
    for t in [-300, -299, -260, -251] {
        assert!(loc(&f, f.varrow, t), "present at {t}");
    }
    for t in [-301, -250, -249, -220, -201] {
        assert!(!loc(&f, f.varrow, t), "absent at {t}");
    }
    for t in [-200, -199, 0, i64::MAX] {
        assert!(loc(&f, f.varrow, t), "present at {t}");
    }
    // "Who is in Varrow at -220?" is a reverse question; here: Greg's own view
    assert_eq!(f.w.as_of(f.greg, "located_in", Tick(-220)).unwrap(), vec![]);
}

#[test]
fn retract_of_one_value_of_many_attribute_leaves_the_others() {
    let mut f = fx();
    f.w.assert(f.greg, "owns", Value::Ref(f.varrow), at(10))
        .unwrap();
    f.w.assert(f.greg, "owns", Value::Ref(f.ashford), at(10))
        .unwrap();
    f.w.assert(f.greg, "owns", Value::Ref(f.carnath), at(15))
        .unwrap();
    f.w.retract(f.greg, "owns", Value::Ref(f.ashford), at(20))
        .unwrap();
    let owns = |p, t| holds(&f.w, f.greg, "owns", Value::Ref(p), t);
    assert!(owns(f.varrow, 25) && owns(f.carnath, 25) && !owns(f.ashford, 25));
    assert!(owns(f.ashford, 19));
    assert_eq!(f.w.as_of(f.greg, "owns", Tick(25)).unwrap().len(), 2);
    assert_eq!(f.w.as_of(f.greg, "owns", Tick(19)).unwrap().len(), 3);
}

#[test]
fn touching_intervals_retract_and_assert_at_same_tick_different_values() {
    let mut f = fx();
    f.w.assert(f.greg, "located_in", Value::Ref(f.varrow), at(200))
        .unwrap();
    f.w.retract(f.greg, "located_in", Value::Ref(f.varrow), at(250))
        .unwrap();
    f.w.assert(f.greg, "located_in", Value::Ref(f.ashford), at(250))
        .unwrap();
    assert!(loc(&f, f.varrow, 249));
    assert!(!loc(&f, f.ashford, 249));
    assert!(!loc(&f, f.varrow, 250));
    assert!(loc(&f, f.ashford, 250));
    assert_eq!(
        f.w.as_of(f.greg, "located_in", Tick(249)).unwrap(),
        vec![Value::Ref(f.varrow)]
    );
    assert_eq!(
        f.w.as_of(f.greg, "located_in", Tick(250)).unwrap(),
        vec![Value::Ref(f.ashford)]
    );
}

#[test]
fn retract_does_not_affect_other_entities_or_attributes() {
    let mut f = fx();
    let mara = f.w.create_entity("CHARACTER", Some("mara")).unwrap();
    f.w.assert(f.greg, "located_in", Value::Ref(f.varrow), at(0))
        .unwrap();
    f.w.assert(mara, "located_in", Value::Ref(f.varrow), at(0))
        .unwrap();
    f.w.assert(f.greg, "owns", Value::Ref(f.varrow), at(0))
        .unwrap();
    f.w.retract(f.greg, "located_in", Value::Ref(f.varrow), at(5))
        .unwrap();
    assert!(!loc(&f, f.varrow, 5));
    assert!(holds(&f.w, mara, "located_in", Value::Ref(f.varrow), 5));
    assert!(holds(&f.w, f.greg, "owns", Value::Ref(f.varrow), 5));
}

#[test]
fn older_canon_still_sees_value_before_its_retract() {
    let mut f = fx();
    f.w.assert(f.greg, "located_in", Value::Ref(f.varrow), at(-300))
        .unwrap();
    let before = f.w.latest_tx();
    f.w.retract(f.greg, "located_in", Value::Ref(f.varrow), at(-250))
        .unwrap();
    assert!(!loc(&f, f.varrow, -200));
    assert!(f.w.latest_tx() > before);
}

#[test]
fn retract_unknown_attribute_and_wrong_type_are_errors() {
    let mut f = fx();
    assert_eq!(
        f.w.retract(f.greg, "nope", Value::Bool(true), at(0)),
        Err(Error::UnknownAttribute("nope".to_string()))
    );
    assert!(matches!(
        f.w.retract(f.greg, "located_in", Value::I64(1), at(0)),
        Err(Error::WrongValueType { .. })
    ));
}
