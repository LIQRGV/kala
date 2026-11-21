//! Item 10: events. Retracting an event attribute is an error. Events are
//! true forever after their tick. Explicit ticks, auto-commit writes only.

use kala_core::*;

fn schema() -> Schema {
    Schema::new()
        .node_type("CHARACTER")
        .node_type("ITEM")
        .attribute(AttrDef::new(
            "bought",
            ValueType::Ref,
            Kind::Event,
            Cardinality::Many,
        ))
        .attribute(AttrDef::new(
            "owns",
            ValueType::Ref,
            Kind::State,
            Cardinality::Many,
        ))
}

struct Fx {
    w: World,
    edwin: EntityId,
    sword: EntityId,
    shield: EntityId,
}

fn fx() -> Fx {
    let mut w = World::in_memory(schema()).expect("world");
    let edwin = w.create_entity("CHARACTER", Some("edwin")).unwrap();
    let sword = w.create_entity("ITEM", Some("sword")).unwrap();
    let shield = w.create_entity("ITEM", Some("shield")).unwrap();
    Fx {
        w,
        edwin,
        sword,
        shield,
    }
}

fn t(x: i64) -> Option<Tick> {
    Some(Tick(x))
}

fn bought(f: &mut Fx, item: &str, tick: i64) -> WriteResult {
    let item = if item == "sword" { f.sword } else { f.shield };
    f.w.assert(f.edwin, "bought", Value::Ref(item), t(tick))
        .unwrap()
}

fn held(f: &Fx, item: EntityId, tick: i64) -> bool {
    f.w.as_of_value(f.edwin, "bought", &Value::Ref(item), Tick(tick))
        .unwrap()
}

#[test]
fn retract_of_an_asserted_event_is_an_error() {
    let mut f = fx();
    bought(&mut f, "sword", -208);
    let tx = f.w.latest_tx();
    let r = f.w.retract(f.edwin, "bought", Value::Ref(f.sword), t(-190));
    assert_eq!(r, Err(Error::RetractEvent("bought".to_string())));
    assert_eq!(f.w.latest_tx(), tx);
    assert!(held(&f, f.sword, -190));
    assert!(held(&f, f.sword, i64::MAX));
    assert_eq!(f.w.as_of(f.edwin, "bought", Tick(0)).unwrap().len(), 1);
}

#[test]
fn retract_of_a_never_asserted_event_is_still_an_error() {
    let mut f = fx();
    let tx = f.w.latest_tx();
    let r = f.w.retract(f.edwin, "bought", Value::Ref(f.sword), t(0));
    assert_eq!(r, Err(Error::RetractEvent("bought".to_string())));
    assert_eq!(f.w.latest_tx(), tx);
    assert!(
        f.w.as_of(f.edwin, "bought", Tick(i64::MAX))
            .unwrap()
            .is_empty()
    );
}

#[test]
fn retract_of_an_event_at_the_assert_tick_is_retract_event_not_a_conflict() {
    let mut f = fx();
    bought(&mut f, "sword", 5);
    let r = f.w.retract(f.edwin, "bought", Value::Ref(f.sword), t(5));
    assert_eq!(r, Err(Error::RetractEvent("bought".to_string())));
}

#[test]
fn retract_of_an_event_before_its_tick_is_an_error() {
    let mut f = fx();
    bought(&mut f, "sword", 5);
    let r = f.w.retract(f.edwin, "bought", Value::Ref(f.sword), t(1));
    assert_eq!(r, Err(Error::RetractEvent("bought".to_string())));
}

#[test]
fn retract_of_an_event_at_tick_extremes_is_an_error() {
    let mut f = fx();
    for tick in [i64::MIN, 0, i64::MAX] {
        let r = f.w.retract(f.edwin, "bought", Value::Ref(f.sword), t(tick));
        assert_eq!(r, Err(Error::RetractEvent("bought".to_string())));
    }
}

#[test]
fn asserting_an_event_twice_at_the_same_tick_is_noop() {
    let mut f = fx();
    assert_eq!(bought(&mut f, "sword", -208).changed, 1);
    let tx = f.w.latest_tx();
    let r = bought(&mut f, "sword", -208);
    assert_eq!(r.changed, 0);
    assert_eq!(r.tick, Tick(-208));
    assert_eq!(f.w.latest_tx(), tx);
}

#[test]
fn asserting_the_same_event_at_a_later_tick_is_noop() {
    // PM reading of "events are true forever after".
    let mut f = fx();
    bought(&mut f, "sword", -208);
    let tx = f.w.latest_tx();
    let r = bought(&mut f, "sword", -100);
    assert_eq!(r.changed, 0);
    assert_eq!(r.tick, Tick(-100));
    assert_eq!(f.w.latest_tx(), tx);
}

#[test]
fn asserting_an_event_at_an_earlier_tick_prequel_is_stored() {
    let mut f = fx();
    bought(&mut f, "sword", -208);
    let tx = f.w.latest_tx();
    let r = bought(&mut f, "sword", -300);
    assert_eq!(r.changed, 1);
    assert_eq!(r.tick, Tick(-300));
    assert_eq!(f.w.latest_tx(), TxId(tx.0 + 1));
    assert!(!held(&f, f.sword, -301));
    assert!(held(&f, f.sword, -300));
    assert_eq!(f.w.as_of(f.edwin, "bought", Tick(-250)).unwrap().len(), 1);
}

#[test]
fn different_events_are_independent() {
    let mut f = fx();
    bought(&mut f, "sword", -208);
    assert_eq!(bought(&mut f, "shield", -300).changed, 1);
    assert!(!held(&f, f.shield, -301));
    assert!(held(&f, f.shield, -208));
}

#[test]
fn event_is_true_forever_after_its_tick() {
    let mut f = fx();
    bought(&mut f, "sword", 0);
    assert!(!held(&f, f.sword, -1));
    assert!(held(&f, f.sword, 0));
    assert!(held(&f, f.sword, i64::MAX));
}

#[test]
fn retract_of_a_state_attribute_on_the_same_entity_still_works() {
    let mut f = fx();
    bought(&mut f, "sword", -208);
    f.w.assert(f.edwin, "owns", Value::Ref(f.sword), t(-208))
        .unwrap();
    let r =
        f.w.retract(f.edwin, "owns", Value::Ref(f.sword), t(-190))
            .unwrap();
    assert_eq!(r.changed, 1);
    assert!(
        f.w.as_of_value(f.edwin, "owns", &Value::Ref(f.sword), Tick(-191))
            .unwrap()
    );
    assert!(
        !f.w.as_of_value(f.edwin, "owns", &Value::Ref(f.sword), Tick(-190))
            .unwrap()
    );
    // the event is untouched
    assert!(held(&f, f.sword, -190));
}

#[test]
fn retract_of_a_second_never_asserted_event_is_an_error() {
    let mut f = fx();
    assert!(matches!(
        f.w.retract(f.edwin, "bought", Value::Ref(f.shield), t(0)),
        Err(Error::RetractEvent(_))
    ));
}

fn undefined_entity() -> EntityId {
    EntityId::new(NodeTypeId(0), 999).unwrap()
}

#[test]
fn validation_beats_retract_event() {
    let mut f = fx();
    bought(&mut f, "sword", 0);
    let tx = f.w.latest_tx();
    let ghost = undefined_entity();

    // undefined Ref value
    assert_eq!(
        f.w.retract(f.edwin, "bought", Value::Ref(ghost), t(1)),
        Err(Error::UnknownEntity(ghost))
    );
    // wrong-type value
    assert_eq!(
        f.w.retract(f.edwin, "bought", Value::Bool(true), t(1)),
        Err(Error::WrongValueType {
            attr: "bought".to_string(),
            expected: ValueType::Ref,
            got: ValueType::Bool,
        })
    );
    // unknown attribute
    assert_eq!(
        f.w.retract(f.edwin, "nope", Value::Ref(f.sword), t(1)),
        Err(Error::UnknownAttribute("nope".to_string()))
    );
    // undefined subject
    assert_eq!(
        f.w.retract(ghost, "bought", Value::Ref(f.sword), t(1)),
        Err(Error::UnknownEntity(ghost))
    );
    assert_eq!(f.w.latest_tx(), tx);
}

#[test]
fn latest_tx_unchanged_after_retract_event_errors_at_extremes() {
    let mut f = fx();
    bought(&mut f, "sword", 0);
    let tx = f.w.latest_tx();
    for tick in [i64::MIN, i64::MIN + 1, -1, 0, 1, i64::MAX - 1, i64::MAX] {
        let r = f.w.retract(f.edwin, "bought", Value::Ref(f.sword), t(tick));
        assert_eq!(r, Err(Error::RetractEvent("bought".to_string())));
        assert_eq!(f.w.latest_tx(), tx);
    }
    assert!(held(&f, f.sword, 0));
    assert!(!held(&f, f.sword, -1));
}
