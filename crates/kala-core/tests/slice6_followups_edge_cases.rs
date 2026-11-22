//! Slice 6 item 00: edge cases found while confirming (CONFIRM step).

use kala_core::*;

fn schema() -> Schema {
    Schema::new()
        .node_type("CHARACTER")
        .node_type("PLACE")
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
}

#[test]
fn edge_read_as_a_subject_before_its_define_is_unknown_entity() {
    let mut w = World::in_memory(schema()).unwrap();
    let a = w.create_entity("CHARACTER", Some("a")).unwrap();
    let p = w.create_entity("PLACE", Some("p")).unwrap();
    let before = w.latest_tx();
    w.assert(a, "present", Value::Ref(p), Some(Tick(1)))
        .unwrap();
    let edge: EntityId = w.edge_id(a, "present", p).unwrap().into();
    let def_tx = w.latest_tx();
    w.assert(edge, "disguised", Value::Bool(true), Some(Tick(1)))
        .unwrap();
    for n in 0..=before.0 {
        let v = w.at_canon(TxId(n));
        assert_eq!(
            v.as_of(edge, "disguised", Tick(1)),
            Err(Error::UnknownEntity(edge)),
            "as_of canon {n}"
        );
        assert_eq!(
            v.history(edge, "disguised"),
            Err(Error::UnknownEntity(edge)),
            "history canon {n}"
        );
        assert_eq!(
            v.as_of_value(edge, "disguised", &Value::Bool(true), Tick(1)),
            Err(Error::UnknownEntity(edge)),
            "as_of_value canon {n}"
        );
    }
    // at the defining tx the edge exists, with no property yet
    let v = w.at_canon(def_tx);
    assert_eq!(v.as_of(edge, "disguised", Tick(1)), Ok(vec![]));
    assert_eq!(v.history(edge, "disguised"), Ok(vec![]));
    // and later it reads
    assert_eq!(
        w.as_of(edge, "disguised", Tick(1)),
        Ok(vec![Value::Bool(true)])
    );
}

#[test]
fn as_of_value_with_a_ref_to_an_entity_defined_after_the_canon_is_false() {
    // Pin: only the subject is checked; a Ref value naming a not-yet-defined
    // entity is simply "not held" (Ok(false)), not an error.
    let mut w = World::in_memory(schema()).unwrap();
    let a = w.create_entity("CHARACTER", Some("a")).unwrap();
    let before = w.latest_tx();
    let p = w.create_entity("PLACE", Some("p")).unwrap();
    w.assert(a, "present", Value::Ref(p), Some(Tick(1)))
        .unwrap();
    for n in 0..=before.0 {
        let v = w.at_canon(TxId(n));
        if n >= 1 {
            assert_eq!(
                v.as_of_value(a, "present", &Value::Ref(p), Tick(1)),
                Ok(false),
                "canon {n}"
            );
        }
    }
    assert_eq!(
        w.at_canon(before)
            .as_of_value(a, "present", &Value::Ref(p), Tick(1)),
        Ok(false)
    );
    assert_eq!(
        w.as_of_value(a, "present", &Value::Ref(p), Tick(1)),
        Ok(true)
    );
}

#[test]
fn must_escape_pins_nbsp_zwsp_and_noncharacters() {
    assert!(!must_escape('\u{00A0}'), "NBSP is visible spacing");
    assert!(must_escape('\u{200B}'), "ZWSP is escaped (slice 7)");
    // pinned: U+FFFE is a Unicode noncharacter, not `Cc`, not in the list
    assert!(!must_escape('\u{FFFE}'));
    assert!(!must_escape('\u{FFFF}'));
}
