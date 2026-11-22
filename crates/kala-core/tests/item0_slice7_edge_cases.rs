//! Slice 7 item 0: edge cases for `history_scan` and `holds_during`.
use kala_core::*;

fn schema() -> Schema {
    let a = |n, t, k, c| AttrDef::new(n, t, k, c);
    Schema::new()
        .node_type("CHARACTER")
        .node_type("PLACE")
        .attribute(a("located_in", ValueType::Ref, Kind::State, Cardinality::One).directed())
        .attribute(
            a("parent_of", ValueType::Ref, Kind::State, Cardinality::Many)
                .directed()
                .inverse("child_of"),
        )
        .attribute(a("bought", ValueType::Ref, Kind::Event, Cardinality::Many).directed())
        .attribute(a("weight", ValueType::F64, Kind::State, Cardinality::One))
}

fn tk(x: i64) -> Option<Tick> {
    Some(Tick(x))
}

#[test]
fn holds_during_f64_negative_zero_equals_zero() {
    let mut w = World::in_memory(schema()).unwrap();
    let e = w.create_entity("CHARACTER", None).unwrap();
    w.assert(e, "weight", Value::F64(0.0), tk(1)).unwrap();
    w.retract(e, "weight", Value::F64(-0.0), tk(5)).unwrap();
    let a = w.holds_during(e, "weight", &Value::F64(0.0)).unwrap();
    let b = w.holds_during(e, "weight", &Value::F64(-0.0)).unwrap();
    assert_eq!(a, b);
    assert!(a.contains(Tick(1)) && a.contains(Tick(4)));
    assert!(!a.contains(Tick(5)) && !a.contains(Tick(0)));
}

#[test]
fn holds_during_entity_defined_after_canon_is_unknown_entity() {
    let mut w = World::in_memory(schema()).unwrap();
    let _a = w.create_entity("CHARACTER", None).unwrap();
    let old = w.latest_tx();
    let late = w.create_entity("CHARACTER", None).unwrap();
    let r = w
        .at_canon(old)
        .holds_during(late, "located_in", &Value::Ref(late));
    assert!(
        matches!(r, Err(Error::UnknownEntity(e)) if e == late),
        "{r:?}"
    );
}

#[test]
fn holds_during_inverse_name_is_unknown_attribute() {
    let mut w = World::in_memory(schema()).unwrap();
    let a = w.create_entity("CHARACTER", None).unwrap();
    let b = w.create_entity("CHARACTER", None).unwrap();
    w.assert(a, "parent_of", Value::Ref(b), tk(0)).unwrap();
    let r = w.holds_during(b, "child_of", &Value::Ref(a));
    assert!(matches!(r, Err(Error::UnknownAttribute(_))), "{r:?}");
    assert!(matches!(
        w.history(b, "child_of"),
        Err(Error::UnknownAttribute(_))
    ));
}

#[test]
fn history_scan_event_attribute_is_asserts_only() {
    let mut w = World::in_memory(schema()).unwrap();
    let a = w.create_entity("CHARACTER", None).unwrap();
    let p = w.create_entity("PLACE", None).unwrap();
    w.assert(a, "bought", Value::Ref(p), tk(-208)).unwrap();
    w.assert(a, "bought", Value::Ref(p), tk(-100)).unwrap();
    let got = w.history_scan("bought").unwrap();
    assert!(!got.is_empty());
    assert!(got.iter().all(|(e, c)| *e == a && c.op == Op::Assert));
    assert_eq!(
        got.iter().map(|(_, c)| c.clone()).collect::<Vec<_>>(),
        w.history(a, "bought").unwrap()
    );
}

#[test]
fn history_scan_after_deleting_every_fact_of_an_entity() {
    let mut w = World::in_memory(schema()).unwrap();
    let a = w.create_entity("CHARACTER", None).unwrap();
    let b = w.create_entity("CHARACTER", None).unwrap();
    let p = w.create_entity("PLACE", None).unwrap();
    let r1 = w
        .assert(a, "located_in", Value::Ref(p), tk(0))
        .unwrap()
        .entries[0];
    let r2 = w
        .retract(a, "located_in", Value::Ref(p), tk(5))
        .unwrap()
        .entries[0];
    w.assert(b, "located_in", Value::Ref(p), tk(1)).unwrap();
    let before = w.latest_tx();
    w.correct(r1, Correction::Delete).unwrap();
    w.correct(r2, Correction::Delete).unwrap();
    let now = w.history_scan("located_in").unwrap();
    assert_eq!(now.iter().map(|(e, _)| *e).collect::<Vec<_>>(), vec![b]);
    let old = w.at_canon(before).history_scan("located_in").unwrap();
    let ents: Vec<_> = old.iter().map(|(e, _)| *e).collect();
    assert_eq!(ents, vec![a, a, b]);
}
