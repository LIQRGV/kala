//! Item 5: assert + AsOf. Only asserts are written here (retract is item 6);
//! no changed counts (7), same-tick conflicts (8), tick defaults (9) or
//! transactions (11). Every write passes an explicit tick.
//!
//! Expected API (api-contract.md):
//!   World::as_of_value(&self, e: EntityId, attr: &str, v: &Value, t: Tick) -> Result<bool, Error>
//!   World::as_of(&self, e: EntityId, attr: &str, t: Tick) -> Result<Vec<Value>, Error>
//! `as_of` returns every value holding at `t`, sorted by value encoding.

use kala_core::*;

fn schema() -> Schema {
    // located_in / owns are declared back to back so their AttrIds are adjacent.
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
            "age",
            ValueType::I64,
            Kind::State,
            Cardinality::One,
        ))
        .attribute(AttrDef::new(
            "scores",
            ValueType::I64,
            Kind::State,
            Cardinality::Many,
        ))
        .attribute(AttrDef::new(
            "height",
            ValueType::F64,
            Kind::State,
            Cardinality::One,
        ))
        .attribute(AttrDef::new(
            "marks",
            ValueType::F64,
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
            "alias",
            ValueType::Str,
            Kind::State,
            Cardinality::Many,
        ))
        .attribute(
            AttrDef::new("biography", ValueType::Str, Kind::State, Cardinality::One).unindexed(),
        )
        .attribute(AttrDef::new(
            "bought",
            ValueType::Ref,
            Kind::Event,
            Cardinality::Many,
        ))
}

struct Fx {
    w: World,
    greg: EntityId,
    edwin: EntityId,
    varrow: EntityId,
    ashford: EntityId,
    carnath: EntityId,
}

fn fx() -> Fx {
    let mut w = World::in_memory(schema()).expect("world");
    let greg = w.create_entity("CHARACTER", Some("greg")).unwrap();
    let edwin = w.create_entity("CHARACTER", Some("edwin")).unwrap();
    let varrow = w.create_entity("PLACE", Some("varrow")).unwrap();
    let ashford = w.create_entity("PLACE", Some("ashford")).unwrap();
    let carnath = w.create_entity("PLACE", Some("carnath")).unwrap();
    Fx {
        w,
        greg,
        edwin,
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

// ---- boundaries around the assert tick ----

#[test]
fn before_first_fact_is_false_and_empty() {
    let mut f = fx();
    f.w.assert(f.greg, "located_in", Value::Ref(f.varrow), at(-300))
        .unwrap();
    assert!(!holds(
        &f.w,
        f.greg,
        "located_in",
        Value::Ref(f.varrow),
        -301
    ));
    assert!(!holds(
        &f.w,
        f.greg,
        "located_in",
        Value::Ref(f.varrow),
        i64::MIN
    ));
    assert_eq!(f.w.as_of(f.greg, "located_in", Tick(-301)).unwrap(), vec![]);
    assert_eq!(
        f.w.as_of(f.greg, "located_in", Tick(i64::MIN)).unwrap(),
        vec![]
    );
}

#[test]
fn never_written_attribute_is_false_and_empty_at_any_tick() {
    let f = fx();
    for t in [i64::MIN, -1, 0, 1, i64::MAX] {
        assert!(!holds(&f.w, f.greg, "alive", Value::Bool(true), t));
        assert!(!holds(&f.w, f.greg, "alive", Value::Bool(false), t));
        assert_eq!(f.w.as_of(f.greg, "alive", Tick(t)).unwrap(), vec![]);
    }
}

#[test]
fn holds_exactly_at_assert_tick_and_after() {
    let mut f = fx();
    f.w.assert(f.greg, "located_in", Value::Ref(f.varrow), at(-300))
        .unwrap();
    let v = || Value::Ref(f.varrow);
    assert!(!holds(&f.w, f.greg, "located_in", v(), -301));
    for t in [-300, -299, -1, 0, 1, 500, i64::MAX] {
        assert!(holds(&f.w, f.greg, "located_in", v(), t), "tick {t}");
        assert_eq!(f.w.as_of(f.greg, "located_in", Tick(t)).unwrap(), vec![v()]);
    }
}

#[test]
fn assert_at_i64_min_holds_everywhere() {
    let mut f = fx();
    f.w.assert(f.greg, "age", Value::I64(7), at(i64::MIN))
        .unwrap();
    for t in [i64::MIN, i64::MIN + 1, -1, 0, i64::MAX] {
        assert!(holds(&f.w, f.greg, "age", Value::I64(7), t), "tick {t}");
    }
}

#[test]
fn assert_at_i64_max_holds_only_at_max() {
    let mut f = fx();
    f.w.assert(f.greg, "age", Value::I64(7), at(i64::MAX))
        .unwrap();
    assert!(!holds(&f.w, f.greg, "age", Value::I64(7), i64::MAX - 1));
    assert!(!holds(&f.w, f.greg, "age", Value::I64(7), i64::MIN));
    assert!(holds(&f.w, f.greg, "age", Value::I64(7), i64::MAX));
    assert_eq!(
        f.w.as_of(f.greg, "age", Tick(i64::MAX)).unwrap(),
        vec![Value::I64(7)]
    );
}

#[test]
fn assert_at_tick_zero_boundary() {
    let mut f = fx();
    f.w.assert(f.greg, "age", Value::I64(1), at(0)).unwrap();
    assert!(!holds(&f.w, f.greg, "age", Value::I64(1), -1));
    assert!(holds(&f.w, f.greg, "age", Value::I64(1), 0));
    assert!(holds(&f.w, f.greg, "age", Value::I64(1), 1));
}

#[test]
fn tick_minus_one_and_zero_are_ordered_across_the_sign_boundary() {
    let mut f = fx();
    f.w.assert(f.greg, "scores", Value::I64(1), at(-1)).unwrap();
    f.w.assert(f.greg, "scores", Value::I64(2), at(0)).unwrap();
    assert_eq!(
        f.w.as_of(f.greg, "scores", Tick(-2)).unwrap(),
        Vec::<Value>::new()
    );
    assert_eq!(
        f.w.as_of(f.greg, "scores", Tick(-1)).unwrap(),
        vec![Value::I64(1)]
    );
    assert_eq!(
        f.w.as_of(f.greg, "scores", Tick(0)).unwrap(),
        vec![Value::I64(1), Value::I64(2)]
    );
}

// ---- cardinality many ----

#[test]
fn many_returns_only_values_holding_at_t_sorted_by_entity_id() {
    let mut f = fx();
    // Written in reverse id order, at increasing ticks.
    f.w.assert(f.greg, "owns", Value::Ref(f.carnath), at(10))
        .unwrap();
    f.w.assert(f.greg, "owns", Value::Ref(f.ashford), at(20))
        .unwrap();
    f.w.assert(f.greg, "owns", Value::Ref(f.varrow), at(30))
        .unwrap();
    let (v, a, c) = (
        Value::Ref(f.varrow),
        Value::Ref(f.ashford),
        Value::Ref(f.carnath),
    );
    assert_eq!(f.w.as_of(f.greg, "owns", Tick(9)).unwrap(), vec![]);
    assert_eq!(
        f.w.as_of(f.greg, "owns", Tick(10)).unwrap(),
        vec![c.clone()]
    );
    // varrow < ashford < carnath by declaration order of ids (seq 1,2,3)
    assert_eq!(
        f.w.as_of(f.greg, "owns", Tick(25)).unwrap(),
        vec![a.clone(), c.clone()]
    );
    assert_eq!(f.w.as_of(f.greg, "owns", Tick(30)).unwrap(), vec![v, a, c]);
}

#[test]
fn many_strings_sorted_by_byte_order() {
    let mut f = fx();
    for (i, s) in ["alpha", "Zed", "é", "alp", "alphabet", ""]
        .into_iter()
        .enumerate()
    {
        f.w.assert(f.greg, "alias", Value::Str(s.into()), at(i as i64))
            .unwrap();
    }
    let got = f.w.as_of(f.greg, "alias", Tick(100)).unwrap();
    let s = |x: &str| Value::Str(x.into());
    // bytewise: "" < "Zed" < "alp" < "alpha" < "alphabet" < "é"
    assert_eq!(
        got,
        vec![s(""), s("Zed"), s("alp"), s("alpha"), s("alphabet"), s("é")]
    );
}

#[test]
fn string_lookup_is_exact_not_prefix() {
    let mut f = fx();
    f.w.assert(f.greg, "alias", Value::Str("alphabet".into()), at(0))
        .unwrap();
    assert!(!holds(&f.w, f.greg, "alias", Value::Str("alpha".into()), 5));
    assert!(!holds(
        &f.w,
        f.greg,
        "alias",
        Value::Str("alphabets".into()),
        5
    ));
    assert!(!holds(&f.w, f.greg, "alias", Value::Str("".into()), 5));
    assert!(holds(
        &f.w,
        f.greg,
        "alias",
        Value::Str("alphabet".into()),
        5
    ));
}

#[test]
fn strings_containing_nul_bytes_stay_distinct() {
    let mut f = fx();
    f.w.assert(f.greg, "alias", Value::Str("a\0b".into()), at(0))
        .unwrap();
    f.w.assert(f.greg, "alias", Value::Str("a".into()), at(1))
        .unwrap();
    assert!(holds(&f.w, f.greg, "alias", Value::Str("a\0b".into()), 1));
    assert!(holds(&f.w, f.greg, "alias", Value::Str("a".into()), 1));
    assert!(!holds(&f.w, f.greg, "alias", Value::Str("a\0".into()), 1));
    assert_eq!(
        f.w.as_of(f.greg, "alias", Tick(1)).unwrap(),
        vec![Value::Str("a".into()), Value::Str("a\0b".into())]
    );
}

#[test]
fn many_i64_sorted_numerically_including_negatives() {
    let mut f = fx();
    for (i, n) in [5, -5, i64::MAX, i64::MIN, 0, -1].into_iter().enumerate() {
        f.w.assert(f.greg, "scores", Value::I64(n), at(i as i64))
            .unwrap();
    }
    assert_eq!(
        f.w.as_of(f.greg, "scores", Tick(10)).unwrap(),
        vec![
            Value::I64(i64::MIN),
            Value::I64(-5),
            Value::I64(-1),
            Value::I64(0),
            Value::I64(5),
            Value::I64(i64::MAX)
        ]
    );
}

#[test]
fn many_f64_sorted_numerically() {
    let mut f = fx();
    for (i, x) in [1.5, -2.5, f64::INFINITY, f64::NEG_INFINITY, 0.0]
        .into_iter()
        .enumerate()
    {
        f.w.assert(f.greg, "marks", Value::F64(x), at(i as i64))
            .unwrap();
    }
    assert_eq!(
        f.w.as_of(f.greg, "marks", Tick(10)).unwrap(),
        vec![
            Value::F64(f64::NEG_INFINITY),
            Value::F64(-2.5),
            Value::F64(0.0),
            Value::F64(1.5),
            Value::F64(f64::INFINITY)
        ]
    );
}

// ---- cardinality one is a label ----

#[test]
fn cardinality_one_assert_never_ends_another_value() {
    let mut f = fx();
    f.w.assert(f.greg, "located_in", Value::Ref(f.varrow), at(200))
        .unwrap();
    f.w.assert(f.greg, "located_in", Value::Ref(f.ashford), at(601))
        .unwrap();
    let (v, a) = (Value::Ref(f.varrow), Value::Ref(f.ashford));
    assert_eq!(
        f.w.as_of(f.greg, "located_in", Tick(700)).unwrap(),
        vec![v.clone(), a.clone()]
    );
    assert!(holds(&f.w, f.greg, "located_in", v.clone(), 700));
    assert!(holds(&f.w, f.greg, "located_in", a.clone(), 700));
    // before the second assert only the first holds
    assert_eq!(f.w.as_of(f.greg, "located_in", Tick(600)).unwrap(), vec![v]);
    assert_eq!(f.w.as_of(f.greg, "located_in", Tick(199)).unwrap(), vec![]);
}

#[test]
fn same_value_asserted_at_two_ticks_is_one_value_in_as_of() {
    let mut f = fx();
    f.w.assert(f.greg, "located_in", Value::Ref(f.varrow), at(10))
        .unwrap();
    f.w.assert(f.greg, "located_in", Value::Ref(f.varrow), at(20))
        .unwrap();
    assert_eq!(
        f.w.as_of(f.greg, "located_in", Tick(25)).unwrap(),
        vec![Value::Ref(f.varrow)]
    );
}

// ---- out-of-order writes ----

#[test]
fn out_of_order_writes_are_visible_at_their_ticks() {
    let mut f = fx();
    f.w.assert(f.greg, "located_in", Value::Ref(f.ashford), at(-100))
        .unwrap();
    // prequel, written later
    f.w.assert(f.greg, "located_in", Value::Ref(f.varrow), at(-208))
        .unwrap();
    let (v, a) = (Value::Ref(f.varrow), Value::Ref(f.ashford));
    assert_eq!(f.w.as_of(f.greg, "located_in", Tick(-209)).unwrap(), vec![]);
    assert_eq!(
        f.w.as_of(f.greg, "located_in", Tick(-208)).unwrap(),
        vec![v.clone()]
    );
    assert_eq!(
        f.w.as_of(f.greg, "located_in", Tick(-101)).unwrap(),
        vec![v.clone()]
    );
    assert!(!holds(&f.w, f.greg, "located_in", a.clone(), -101));
    assert!(holds(&f.w, f.greg, "located_in", a.clone(), -100));
    assert_eq!(
        f.w.as_of(f.greg, "located_in", Tick(-100)).unwrap(),
        vec![v, a]
    );
}

#[test]
fn same_value_prequel_written_later_moves_start_earlier() {
    let mut f = fx();
    f.w.assert(f.greg, "alive", Value::Bool(true), at(50))
        .unwrap();
    f.w.assert(f.greg, "alive", Value::Bool(true), at(-50))
        .unwrap();
    assert!(!holds(&f.w, f.greg, "alive", Value::Bool(true), -51));
    assert!(holds(&f.w, f.greg, "alive", Value::Bool(true), -50));
    assert!(holds(&f.w, f.greg, "alive", Value::Bool(true), 0));
}

#[test]
fn two_values_at_the_same_tick_both_hold() {
    let mut f = fx();
    f.w.assert(f.greg, "owns", Value::Ref(f.varrow), at(5))
        .unwrap();
    f.w.assert(f.greg, "owns", Value::Ref(f.ashford), at(5))
        .unwrap();
    assert_eq!(
        f.w.as_of(f.greg, "owns", Tick(5)).unwrap(),
        vec![Value::Ref(f.varrow), Value::Ref(f.ashford)]
    );
}

// ---- isolation between entities / attributes / values ----

#[test]
fn different_entities_do_not_leak() {
    let mut f = fx();
    // greg and edwin are CHARACTER#1 and #2: adjacent IDs.
    assert_eq!(f.greg.seq() + 1, f.edwin.seq());
    f.w.assert(f.greg, "located_in", Value::Ref(f.varrow), at(0))
        .unwrap();
    assert!(!holds(
        &f.w,
        f.edwin,
        "located_in",
        Value::Ref(f.varrow),
        10
    ));
    assert_eq!(f.w.as_of(f.edwin, "located_in", Tick(10)).unwrap(), vec![]);
    f.w.assert(f.edwin, "located_in", Value::Ref(f.ashford), at(0))
        .unwrap();
    assert_eq!(
        f.w.as_of(f.greg, "located_in", Tick(10)).unwrap(),
        vec![Value::Ref(f.varrow)]
    );
    assert_eq!(
        f.w.as_of(f.edwin, "located_in", Tick(10)).unwrap(),
        vec![Value::Ref(f.ashford)]
    );
}

#[test]
fn entities_of_different_types_do_not_leak() {
    let mut f = fx();
    // The last CHARACTER and the first PLACE are neighbours in key order.
    f.w.assert(f.edwin, "alive", Value::Bool(true), at(0))
        .unwrap();
    assert_eq!(f.w.as_of(f.varrow, "alive", Tick(10)).unwrap(), vec![]);
    f.w.assert(f.varrow, "alias", Value::Str("The Pass".into()), at(0))
        .unwrap();
    assert_eq!(f.w.as_of(f.edwin, "alias", Tick(10)).unwrap(), vec![]);
}

#[test]
fn different_attributes_do_not_leak() {
    let mut f = fx();
    // located_in and owns are adjacent AttrIds and share a value type.
    f.w.assert(f.greg, "located_in", Value::Ref(f.varrow), at(0))
        .unwrap();
    assert!(!holds(&f.w, f.greg, "owns", Value::Ref(f.varrow), 10));
    assert_eq!(f.w.as_of(f.greg, "owns", Tick(10)).unwrap(), vec![]);
    f.w.assert(f.greg, "owns", Value::Ref(f.ashford), at(0))
        .unwrap();
    assert_eq!(
        f.w.as_of(f.greg, "located_in", Tick(10)).unwrap(),
        vec![Value::Ref(f.varrow)]
    );
    assert_eq!(
        f.w.as_of(f.greg, "owns", Tick(10)).unwrap(),
        vec![Value::Ref(f.ashford)]
    );
}

#[test]
fn same_encoded_payload_in_different_attributes_does_not_leak() {
    // i64 1 vs bool true vs the same tick: separate attributes, separate keys.
    let mut f = fx();
    f.w.assert(f.greg, "age", Value::I64(1), at(0)).unwrap();
    assert_eq!(f.w.as_of(f.greg, "scores", Tick(0)).unwrap(), vec![]);
    assert!(!holds(&f.w, f.greg, "scores", Value::I64(1), 0));
}

#[test]
fn neighbouring_values_do_not_leak() {
    let mut f = fx();
    f.w.assert(f.greg, "age", Value::I64(41), at(0)).unwrap();
    f.w.assert(f.greg, "age", Value::I64(43), at(0)).unwrap();
    assert!(!holds(&f.w, f.greg, "age", Value::I64(42), 10));
    assert!(holds(&f.w, f.greg, "age", Value::I64(41), 10));
    assert!(holds(&f.w, f.greg, "age", Value::I64(43), 10));
}

// ---- every value type ----

#[test]
fn bool_values() {
    let mut f = fx();
    f.w.assert(f.greg, "alive", Value::Bool(true), at(0))
        .unwrap();
    assert!(holds(&f.w, f.greg, "alive", Value::Bool(true), 0));
    assert!(!holds(&f.w, f.greg, "alive", Value::Bool(false), 0));
    assert_eq!(
        f.w.as_of(f.greg, "alive", Tick(0)).unwrap(),
        vec![Value::Bool(true)]
    );
}

#[test]
fn i64_values() {
    let mut f = fx();
    f.w.assert(f.greg, "age", Value::I64(-42), at(0)).unwrap();
    assert!(holds(&f.w, f.greg, "age", Value::I64(-42), 0));
    assert!(!holds(&f.w, f.greg, "age", Value::I64(42), 0));
    assert_eq!(
        f.w.as_of(f.greg, "age", Tick(0)).unwrap(),
        vec![Value::I64(-42)]
    );
}

#[test]
fn f64_values() {
    let mut f = fx();
    f.w.assert(f.greg, "height", Value::F64(1.82), at(0))
        .unwrap();
    assert!(holds(&f.w, f.greg, "height", Value::F64(1.82), 0));
    assert!(!holds(&f.w, f.greg, "height", Value::F64(1.83), 0));
    assert_eq!(
        f.w.as_of(f.greg, "height", Tick(0)).unwrap(),
        vec![Value::F64(1.82)]
    );
}

#[test]
fn negative_zero_is_read_as_zero() {
    let mut f = fx();
    f.w.assert(f.greg, "height", Value::F64(-0.0), at(0))
        .unwrap();
    assert!(holds(&f.w, f.greg, "height", Value::F64(0.0), 0));
    assert!(holds(&f.w, f.greg, "height", Value::F64(-0.0), 0));
    let got = f.w.as_of(f.greg, "height", Tick(0)).unwrap();
    assert_eq!(got.len(), 1);
    match got[0] {
        Value::F64(x) => assert!(x == 0.0 && x.is_sign_positive(), "got {x:?}"),
        ref other => panic!("{other:?}"),
    }
}

#[test]
fn zero_and_negative_zero_are_one_value() {
    let mut f = fx();
    f.w.assert(f.greg, "marks", Value::F64(0.0), at(0)).unwrap();
    f.w.assert(f.greg, "marks", Value::F64(-0.0), at(1))
        .unwrap();
    assert_eq!(f.w.as_of(f.greg, "marks", Tick(5)).unwrap().len(), 1);
}

#[test]
fn string_values_indexed_and_unindexed() {
    let mut f = fx();
    f.w.assert(f.greg, "name", Value::Str("Edwin".into()), at(0))
        .unwrap();
    assert!(holds(&f.w, f.greg, "name", Value::Str("Edwin".into()), 0));
    assert!(!holds(&f.w, f.greg, "name", Value::Str("edwin".into()), 0)); // exact bytes
    let bio = "b".repeat(10_000);
    f.w.assert(f.greg, "biography", Value::Str(bio.clone()), at(0))
        .unwrap();
    assert!(holds(&f.w, f.greg, "biography", Value::Str(bio.clone()), 0));
    assert_eq!(
        f.w.as_of(f.greg, "biography", Tick(0)).unwrap(),
        vec![Value::Str(bio)]
    );
}

#[test]
fn ref_values() {
    let mut f = fx();
    f.w.assert(f.greg, "owns", Value::Ref(f.edwin), at(0))
        .unwrap(); // different type
    f.w.assert(f.greg, "owns", Value::Ref(f.greg), at(0))
        .unwrap(); // self
    assert!(holds(&f.w, f.greg, "owns", Value::Ref(f.edwin), 0));
    assert!(holds(&f.w, f.greg, "owns", Value::Ref(f.greg), 0));
    assert_eq!(
        f.w.as_of(f.greg, "owns", Tick(0)).unwrap(),
        vec![Value::Ref(f.greg), Value::Ref(f.edwin)]
    );
}

// ---- events ----

#[test]
fn event_attribute_holds_at_its_tick_and_forever_after() {
    let mut f = fx();
    f.w.assert(f.edwin, "bought", Value::Ref(f.varrow), at(-208))
        .unwrap();
    let v = Value::Ref(f.varrow);
    assert!(!holds(&f.w, f.edwin, "bought", v.clone(), -209));
    for t in [-208, -207, 0, i64::MAX] {
        assert!(holds(&f.w, f.edwin, "bought", v.clone(), t), "tick {t}");
        assert_eq!(
            f.w.as_of(f.edwin, "bought", Tick(t)).unwrap(),
            vec![v.clone()]
        );
    }
}

// ---- read errors ----

#[test]
fn reads_with_unknown_attribute_are_errors() {
    let f = fx();
    let r = f.w.as_of_value(f.greg, "nope", &Value::Bool(true), Tick(0));
    assert!(
        matches!(r, Err(Error::UnknownAttribute(ref a)) if a == "nope"),
        "{r:?}"
    );
    let r = f.w.as_of(f.greg, "nope", Tick(0));
    assert!(
        matches!(r, Err(Error::UnknownAttribute(ref a)) if a == "nope"),
        "{r:?}"
    );
}

#[test]
fn reads_with_undefined_entity_are_errors() {
    let mut f = fx();
    let ghost = EntityId::new(NodeTypeId(0), 999).unwrap();
    let r = f.w.as_of(ghost, "alive", Tick(0));
    assert!(
        matches!(r, Err(Error::UnknownEntity(e)) if e == ghost),
        "{r:?}"
    );
    let r = f.w.as_of_value(ghost, "alive", &Value::Bool(true), Tick(0));
    assert!(
        matches!(r, Err(Error::UnknownEntity(e)) if e == ghost),
        "{r:?}"
    );
    // a type declared but with no entity yet, and an edge-tagged id
    let no_such = EntityId::new(NodeTypeId(1), 50).unwrap();
    assert!(matches!(
        f.w.as_of(no_such, "alive", Tick(0)),
        Err(Error::UnknownEntity(_))
    ));
    let edge = EntityId::from_bytes(((1u64 << 63) | 1).to_be_bytes());
    assert!(matches!(
        f.w.as_of(edge, "alive", Tick(0)),
        Err(Error::UnknownEntity(_))
    ));
    // writes still work afterwards
    f.w.assert(f.greg, "alive", Value::Bool(true), at(0))
        .unwrap();
}

#[test]
fn as_of_does_not_change_the_world() {
    let mut f = fx();
    f.w.assert(f.greg, "alive", Value::Bool(true), at(0))
        .unwrap();
    let tx = f.w.latest_tx();
    let _ = f.w.as_of(f.greg, "alive", Tick(0)).unwrap();
    let _ =
        f.w.as_of_value(f.greg, "alive", &Value::Bool(true), Tick(0))
            .unwrap();
    assert_eq!(f.w.latest_tx(), tx);
}

// ---- read-side error / edge handling (api-contract "Decisions") ----

#[test]
fn as_of_value_wrong_type_is_error() {
    let f = fx();
    let r =
        f.w.as_of_value(f.greg, "located_in", &Value::I64(1), Tick(0));
    assert_eq!(
        r,
        Err(Error::WrongValueType {
            attr: "located_in".to_string(),
            expected: ValueType::Ref,
            got: ValueType::I64,
        })
    );
    let r =
        f.w.as_of_value(f.greg, "age", &Value::Str("x".into()), Tick(0));
    assert!(matches!(
        r,
        Err(Error::WrongValueType {
            expected: ValueType::I64,
            got: ValueType::Str,
            ..
        })
    ));
}

#[test]
fn as_of_value_nan_is_value_error() {
    let f = fx();
    let r =
        f.w.as_of_value(f.greg, "height", &Value::F64(f64::NAN), Tick(0));
    assert_eq!(r, Err(Error::Value(ValueError::NaN)));
}

#[test]
fn as_of_value_indexed_string_over_256_bytes_is_false_not_error() {
    let f = fx();
    let long = Value::Str("x".repeat(257));
    assert_eq!(f.w.as_of_value(f.greg, "name", &long, Tick(0)), Ok(false));
    // exactly 256 is an ordinary lookup
    let ok = Value::Str("x".repeat(256));
    assert_eq!(f.w.as_of_value(f.greg, "name", &ok, Tick(0)), Ok(false));
}

#[test]
fn as_of_value_ref_to_undefined_entity_is_false_not_error() {
    let f = fx();
    let ghost = EntityId::new(NodeTypeId(1), 999).unwrap();
    assert_eq!(
        f.w.as_of_value(f.greg, "located_in", &Value::Ref(ghost), Tick(0)),
        Ok(false)
    );
}
