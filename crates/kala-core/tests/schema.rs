//! Item 4: schema (create-time only). Declared node types and attributes;
//! unknown attributes, wrong-typed values and bad values are write errors.
//! Only Ok-ness / errors are asserted here; reading facts back is item 5.

use kala_core::*;

const T: Option<Tick> = Some(Tick(10));

fn schema() -> Schema {
    Schema::new()
        .node_type("CHARACTER")
        .node_type("PLACE")
        .attribute(AttrDef::new(
            "alive",
            ValueType::Bool,
            Kind::State,
            Cardinality::One,
        ))
        .attribute(AttrDef::new(
            "age",
            ValueType::I64,
            Kind::State,
            Cardinality::One,
        ))
        .attribute(AttrDef::new(
            "height",
            ValueType::F64,
            Kind::State,
            Cardinality::One,
        ))
        .attribute(AttrDef::new(
            "name",
            ValueType::Str,
            Kind::State,
            Cardinality::One,
        ))
        .attribute(
            AttrDef::new("biography", ValueType::Str, Kind::State, Cardinality::One).unindexed(),
        )
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
            "alias",
            ValueType::Str,
            Kind::State,
            Cardinality::Many,
        ))
        .attribute(AttrDef::new(
            "bought",
            ValueType::Ref,
            Kind::Event,
            Cardinality::Many,
        ))
}

struct Fx {
    w: World,
    edwin: EntityId,
    varrow: EntityId,
}

fn fx() -> Fx {
    let mut w = World::in_memory(schema()).expect("world");
    let edwin = w.create_entity("CHARACTER", Some("edwin")).unwrap();
    let varrow = w.create_entity("PLACE", Some("varrow")).unwrap();
    Fx { w, edwin, varrow }
}

// ---- unknown attribute ----

#[test]
fn assert_unknown_attribute_is_error() {
    let mut f = fx();
    let r = f.w.assert(f.edwin, "nope", Value::Bool(true), T);
    assert!(
        matches!(r, Err(Error::UnknownAttribute(ref a)) if a == "nope"),
        "{r:?}"
    );
}

#[test]
fn retract_unknown_attribute_is_error() {
    let mut f = fx();
    let r = f.w.retract(f.edwin, "nope", Value::Bool(true), T);
    assert!(
        matches!(r, Err(Error::UnknownAttribute(ref a)) if a == "nope"),
        "{r:?}"
    );
}

#[test]
fn attribute_names_are_exact() {
    let mut f = fx();
    let r = f.w.assert(f.edwin, "Name", Value::Str("Edwin".into()), T);
    assert!(matches!(r, Err(Error::UnknownAttribute(_))), "{r:?}");
}

// ---- wrong value type ----

fn assert_wrong_type(attr: &str, v: Value, expected: ValueType, got: ValueType) {
    let mut f = fx();
    let r = f.w.assert(f.edwin, attr, v.clone(), T);
    match r {
        Err(Error::WrongValueType {
            attr: a,
            expected: e,
            got: g,
        }) => {
            assert_eq!(a, attr);
            assert_eq!(e, expected);
            assert_eq!(g, got);
        }
        other => panic!("expected WrongValueType, got {other:?}"),
    }
    // retract is judged the same way
    let r = f.w.retract(f.edwin, attr, v, T);
    assert!(matches!(r, Err(Error::WrongValueType { .. })), "{r:?}");
}

#[test]
fn i64_into_string_attribute_is_wrong_type() {
    assert_wrong_type("name", Value::I64(3), ValueType::Str, ValueType::I64);
}

#[test]
fn string_into_ref_attribute_is_wrong_type() {
    assert_wrong_type(
        "located_in",
        Value::Str("Varrow".into()),
        ValueType::Ref,
        ValueType::Str,
    );
}

#[test]
fn bool_into_f64_attribute_is_wrong_type() {
    assert_wrong_type("height", Value::Bool(true), ValueType::F64, ValueType::Bool);
}

#[test]
fn f64_into_i64_attribute_is_wrong_type() {
    // 3 and 3.0 never meet in one attribute.
    assert_wrong_type("age", Value::F64(3.0), ValueType::I64, ValueType::F64);
}

#[test]
fn i64_into_f64_attribute_is_wrong_type() {
    assert_wrong_type("height", Value::I64(3), ValueType::F64, ValueType::I64);
}

#[test]
fn ref_into_bool_attribute_is_wrong_type() {
    let f = fx();
    assert_wrong_type(
        "alive",
        Value::Ref(f.varrow),
        ValueType::Bool,
        ValueType::Ref,
    );
}

#[test]
fn wrong_type_on_event_attribute_is_error() {
    let mut f = fx();
    let r = f.w.assert(f.edwin, "bought", Value::I64(1), T);
    assert!(matches!(r, Err(Error::WrongValueType { .. })), "{r:?}");
}

// ---- bad values ----

#[test]
fn nan_is_rejected() {
    let mut f = fx();
    let r = f.w.assert(f.edwin, "height", Value::F64(f64::NAN), T);
    assert!(matches!(r, Err(Error::Value(ValueError::NaN))), "{r:?}");
    let r = f.w.retract(f.edwin, "height", Value::F64(f64::NAN), T);
    assert!(matches!(r, Err(Error::Value(ValueError::NaN))), "{r:?}");
}

#[test]
fn infinities_and_negative_zero_are_accepted() {
    let mut f = fx();
    f.w.assert(f.edwin, "height", Value::F64(f64::INFINITY), T)
        .unwrap();
    f.w.assert(f.edwin, "height", Value::F64(f64::NEG_INFINITY), T)
        .unwrap();
    f.w.assert(f.edwin, "height", Value::F64(-0.0), T).unwrap();
}

#[test]
fn indexed_string_of_256_bytes_is_ok() {
    let mut f = fx();
    let s = "a".repeat(256);
    f.w.assert(f.edwin, "name", Value::Str(s), T).unwrap();
}

#[test]
fn indexed_string_of_257_bytes_is_error() {
    let mut f = fx();
    let r = f.w.assert(f.edwin, "name", Value::Str("a".repeat(257)), T);
    assert!(
        matches!(r, Err(Error::Value(ValueError::StringTooLong { len: 257 }))),
        "{r:?}"
    );
}

#[test]
fn indexed_string_limit_counts_utf8_bytes_not_chars() {
    let mut f = fx();
    // 128 two-byte chars = 256 bytes: ok. 129 = 258 bytes: error.
    f.w.assert(f.edwin, "name", Value::Str("é".repeat(128)), T)
        .unwrap();
    let r = f.w.assert(f.edwin, "name", Value::Str("é".repeat(129)), T);
    assert!(
        matches!(r, Err(Error::Value(ValueError::StringTooLong { len: 258 }))),
        "{r:?}"
    );
}

#[test]
fn indexed_string_limit_applies_to_many_attribute_too() {
    let mut f = fx();
    let r = f.w.assert(f.edwin, "alias", Value::Str("a".repeat(257)), T);
    assert!(
        matches!(r, Err(Error::Value(ValueError::StringTooLong { .. }))),
        "{r:?}"
    );
}

#[test]
fn unindexed_string_of_10000_bytes_is_ok() {
    let mut f = fx();
    f.w.assert(f.edwin, "biography", Value::Str("b".repeat(10_000)), T)
        .unwrap();
}

#[test]
fn unindexed_string_still_type_checked() {
    let mut f = fx();
    let r = f.w.assert(f.edwin, "biography", Value::I64(1), T);
    assert!(matches!(r, Err(Error::WrongValueType { .. })), "{r:?}");
}

// ---- unknown entities ----

#[test]
fn subject_entity_not_created_is_error() {
    let mut f = fx();
    let ghost = EntityId::new(NodeTypeId(0), 999).unwrap();
    let r = f.w.assert(ghost, "alive", Value::Bool(true), T);
    assert!(
        matches!(r, Err(Error::UnknownEntity(e)) if e == ghost),
        "{r:?}"
    );
    let r = f.w.retract(ghost, "alive", Value::Bool(true), T);
    assert!(
        matches!(r, Err(Error::UnknownEntity(e)) if e == ghost),
        "{r:?}"
    );
}

#[test]
fn ref_value_pointing_at_undefined_entity_is_error() {
    let mut f = fx();
    let ghost = EntityId::new(NodeTypeId(1), 999).unwrap();
    let r = f.w.assert(f.edwin, "located_in", Value::Ref(ghost), T);
    assert!(
        matches!(r, Err(Error::UnknownEntity(e)) if e == ghost),
        "{r:?}"
    );
}

// ---- happy path per type ----

#[test]
fn correctly_typed_write_for_each_type_is_ok() {
    let mut f = fx();
    f.w.assert(f.edwin, "alive", Value::Bool(true), T).unwrap();
    f.w.assert(f.edwin, "age", Value::I64(-42), T).unwrap();
    f.w.assert(f.edwin, "height", Value::F64(1.82), T).unwrap();
    f.w.assert(f.edwin, "name", Value::Str("Edwin".into()), T)
        .unwrap();
    f.w.assert(f.edwin, "located_in", Value::Ref(f.varrow), T)
        .unwrap();
}

#[test]
fn state_attribute_assert_cardinality_one_and_many_ok() {
    let mut f = fx();
    f.w.assert(f.edwin, "located_in", Value::Ref(f.varrow), T)
        .unwrap();
    f.w.assert(f.edwin, "owns", Value::Ref(f.varrow), T)
        .unwrap();
    f.w.assert(f.edwin, "alias", Value::Str("the Grey King".into()), T)
        .unwrap();
}

#[test]
fn event_attribute_assert_is_ok() {
    let mut f = fx();
    f.w.assert(f.edwin, "bought", Value::Ref(f.varrow), T)
        .unwrap();
}

#[test]
fn state_attribute_retract_with_valid_args_is_ok() {
    // Ok-ness only: whether it changes anything is item 7.
    let mut f = fx();
    f.w.assert(f.edwin, "alive", Value::Bool(true), Some(Tick(1)))
        .unwrap();
    f.w.retract(f.edwin, "alive", Value::Bool(true), Some(Tick(2)))
        .unwrap();
}

#[test]
fn extreme_ticks_are_accepted() {
    let mut f = fx();
    f.w.assert(f.edwin, "age", Value::I64(1), Some(Tick(i64::MIN)))
        .unwrap();
    f.w.assert(f.edwin, "age", Value::I64(2), Some(Tick(i64::MAX)))
        .unwrap();
}

// ---- schema construction ----

#[test]
fn duplicate_attribute_name_is_error() {
    let s = Schema::new()
        .attribute(AttrDef::new(
            "name",
            ValueType::Str,
            Kind::State,
            Cardinality::One,
        ))
        .attribute(AttrDef::new(
            "name",
            ValueType::I64,
            Kind::State,
            Cardinality::Many,
        ));
    assert!(matches!(World::in_memory(s), Err(Error::DuplicateName(ref n)) if n == "name"));
}

#[test]
fn empty_attribute_name_is_invalid() {
    let s = Schema::new().attribute(AttrDef::new(
        "",
        ValueType::Str,
        Kind::State,
        Cardinality::One,
    ));
    assert!(matches!(World::in_memory(s), Err(Error::InvalidName(_))));
}

#[test]
fn attribute_may_share_a_node_type_name() {
    let s = Schema::new().node_type("CHARACTER").attribute(AttrDef::new(
        "CHARACTER",
        ValueType::Bool,
        Kind::State,
        Cardinality::One,
    ));
    let mut w = World::in_memory(s).expect("separate namespaces");
    let e = w.create_entity("CHARACTER", None).unwrap();
    w.assert(e, "CHARACTER", Value::Bool(true), T).unwrap();
}

#[test]
fn attribute_defs_expose_their_fields() {
    let d = AttrDef::new("name", ValueType::Str, Kind::State, Cardinality::One);
    assert_eq!(d.name, "name");
    assert_eq!(d.value_type, ValueType::Str);
    assert_eq!(d.kind, Kind::State);
    assert_eq!(d.cardinality, Cardinality::One);
    assert!(d.indexed);
    let u = d.unindexed();
    assert!(!u.indexed);
}

// ---- failed writes leave the world usable ----

#[test]
fn failed_writes_leave_world_usable() {
    let mut f = fx();
    let ghost = EntityId::new(NodeTypeId(0), 999).unwrap();
    assert!(f.w.assert(f.edwin, "nope", Value::Bool(true), T).is_err());
    assert!(f.w.assert(f.edwin, "age", Value::F64(1.0), T).is_err());
    assert!(
        f.w.assert(f.edwin, "height", Value::F64(f64::NAN), T)
            .is_err()
    );
    assert!(
        f.w.assert(f.edwin, "name", Value::Str("a".repeat(257)), T)
            .is_err()
    );
    assert!(f.w.assert(ghost, "alive", Value::Bool(true), T).is_err());
    assert!(
        f.w.assert(f.edwin, "located_in", Value::Ref(ghost), T)
            .is_err()
    );
    let tx = f.w.latest_tx();
    let r = f.w.assert(f.edwin, "age", Value::I64(30), T).unwrap();
    assert_eq!(r.tick, Tick(10));
    assert_eq!(f.w.latest_tx(), TxId(tx.0 + 1));
}

// ---- item 4 confirm: edge cases ----

fn edge_tagged_id() -> EntityId {
    // bit 63 set (edge tag), otherwise looks like CHARACTER#1.
    let raw: u64 = (1u64 << 63) | 1;
    EntityId::from_bytes(raw.to_be_bytes())
}

#[test]
fn edge_tagged_id_as_subject_is_unknown_entity() {
    let mut f = fx();
    let edge = edge_tagged_id();
    let r = f.w.assert(edge, "alive", Value::Bool(true), T);
    assert!(
        matches!(r, Err(Error::UnknownEntity(e)) if e == edge),
        "{r:?}"
    );
    let r = f.w.retract(edge, "alive", Value::Bool(true), T);
    assert!(
        matches!(r, Err(Error::UnknownEntity(e)) if e == edge),
        "{r:?}"
    );
}

#[test]
fn edge_tagged_id_as_ref_value_is_unknown_entity() {
    let mut f = fx();
    let edge = edge_tagged_id();
    let r = f.w.assert(f.edwin, "located_in", Value::Ref(edge), T);
    assert!(
        matches!(r, Err(Error::UnknownEntity(e)) if e == edge),
        "{r:?}"
    );
}

#[test]
fn edge_tagged_id_with_otherwise_defined_bits_is_still_unknown() {
    // Same low bits as the real edwin, plus the edge tag.
    let f = fx();
    let raw = f.edwin.raw() | (1u64 << 63);
    let edge = EntityId::from_bytes(raw.to_be_bytes());
    let mut f = f;
    let r = f.w.assert(edge, "alive", Value::Bool(true), T);
    assert!(matches!(r, Err(Error::UnknownEntity(_))), "{r:?}");
}

#[test]
fn valid_range_but_undeclared_node_type_is_unknown_entity() {
    // Schema declares 2 node types (0, 1); type 5 is in range but undeclared.
    let mut f = fx();
    let ghost = EntityId::new(NodeTypeId(5), 1).unwrap();
    let r = f.w.assert(ghost, "alive", Value::Bool(true), T);
    assert!(
        matches!(r, Err(Error::UnknownEntity(e)) if e == ghost),
        "{r:?}"
    );
    let r = f.w.assert(f.edwin, "located_in", Value::Ref(ghost), T);
    assert!(
        matches!(r, Err(Error::UnknownEntity(e)) if e == ghost),
        "{r:?}"
    );
}

#[test]
fn seq_equal_to_last_created_ok_and_next_is_unknown() {
    let mut w = World::in_memory(schema()).unwrap();
    let c1 = w.create_entity("CHARACTER", None).unwrap();
    let c2 = w.create_entity("CHARACTER", None).unwrap();
    let c3 = w.create_entity("CHARACTER", None).unwrap();
    assert_eq!(c3.seq(), 3);
    w.assert(c3, "alive", Value::Bool(true), T).unwrap();
    w.assert(c1, "alive", Value::Bool(true), T).unwrap();
    w.assert(c2, "alive", Value::Bool(true), T).unwrap();
    let next = EntityId::new(c3.node_type(), 4).unwrap();
    let r = w.assert(next, "alive", Value::Bool(true), T);
    assert!(
        matches!(r, Err(Error::UnknownEntity(e)) if e == next),
        "{r:?}"
    );
    // the other type's counter is independent: PLACE has none yet.
    let place1 = EntityId::new(NodeTypeId(1), 1).unwrap();
    let r = w.assert(place1, "alive", Value::Bool(true), T);
    assert!(matches!(r, Err(Error::UnknownEntity(_))), "{r:?}");
}

#[test]
fn ref_to_a_different_node_type_is_ok() {
    let mut f = fx();
    // edwin (CHARACTER) located_in varrow (PLACE) and owns another CHARACTER.
    let mara = f.w.create_entity("CHARACTER", None).unwrap();
    f.w.assert(f.edwin, "located_in", Value::Ref(f.varrow), T)
        .unwrap();
    f.w.assert(f.edwin, "owns", Value::Ref(mara), T).unwrap();
}

#[test]
fn ref_to_self_is_ok() {
    let mut f = fx();
    f.w.assert(f.edwin, "owns", Value::Ref(f.edwin), T).unwrap();
}

#[test]
fn unindexed_i64_attribute_still_type_checks() {
    let s = Schema::new().node_type("CHARACTER").attribute(
        AttrDef::new("score", ValueType::I64, Kind::State, Cardinality::One).unindexed(),
    );
    let mut w = World::in_memory(s).unwrap();
    let e = w.create_entity("CHARACTER", None).unwrap();
    w.assert(e, "score", Value::I64(5), T).unwrap();
    let r = w.assert(e, "score", Value::Str("5".into()), T);
    assert!(
        matches!(
            r,
            Err(Error::WrongValueType {
                expected: ValueType::I64,
                got: ValueType::Str,
                ..
            })
        ),
        "{r:?}"
    );
    let r = w.assert(e, "score", Value::F64(5.0), T);
    assert!(matches!(r, Err(Error::WrongValueType { .. })), "{r:?}");
}

#[test]
fn attribute_names_differing_only_in_case_are_distinct() {
    let s = Schema::new()
        .node_type("CHARACTER")
        .attribute(AttrDef::new(
            "name",
            ValueType::Str,
            Kind::State,
            Cardinality::One,
        ))
        .attribute(AttrDef::new(
            "Name",
            ValueType::I64,
            Kind::State,
            Cardinality::One,
        ));
    let mut w = World::in_memory(s).expect("both allowed");
    let e = w.create_entity("CHARACTER", None).unwrap();
    w.assert(e, "name", Value::Str("x".into()), T).unwrap();
    w.assert(e, "Name", Value::I64(1), T).unwrap();
    // each keeps its own type
    assert!(matches!(
        w.assert(e, "name", Value::I64(1), T),
        Err(Error::WrongValueType { .. })
    ));
    assert!(matches!(
        w.assert(e, "Name", Value::Str("x".into()), T),
        Err(Error::WrongValueType { .. })
    ));
}

#[test]
fn failed_retract_leaves_latest_tx_unchanged() {
    let mut f = fx();
    f.w.assert(f.edwin, "age", Value::I64(1), T).unwrap();
    let tx = f.w.latest_tx();
    assert!(f.w.retract(f.edwin, "nope", Value::Bool(true), T).is_err());
    assert!(f.w.retract(f.edwin, "age", Value::F64(1.0), T).is_err());
    assert!(
        f.w.retract(f.edwin, "height", Value::F64(f64::NAN), T)
            .is_err()
    );
    let ghost = EntityId::new(NodeTypeId(0), 999).unwrap();
    assert!(f.w.retract(ghost, "age", Value::I64(1), T).is_err());
    assert_eq!(f.w.latest_tx(), tx);
}
