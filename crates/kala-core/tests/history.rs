//! Item 12: History. Change points `(tick, op, value)` of one `(entity,
//! attribute)`, in tick order ascending; ties at the same tick ordered by
//! value encoding ascending. Reads only stored, effective writes.
//!
//! Expected API (contract): `World::history(&self, e: EntityId, attr: &str)
//! -> Result<Vec<Change>, Error>`, the same on `View`;
//! `pub struct Change { pub tick: Tick, pub op: Op, pub value: Value }`
//! (Debug, Clone, PartialEq); `pub enum Op { Assert, Retract }` public and
//! re-exported from the crate root (Debug, Clone, Copy, PartialEq, Eq).

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
            "alias",
            ValueType::Str,
            Kind::State,
            Cardinality::Many,
        ))
        .attribute(AttrDef::new(
            "age",
            ValueType::I64,
            Kind::State,
            Cardinality::Many,
        ))
        .attribute(AttrDef::new(
            "height",
            ValueType::F64,
            Kind::State,
            Cardinality::Many,
        ))
        .attribute(AttrDef::new(
            "alive",
            ValueType::Bool,
            Kind::State,
            Cardinality::Many,
        ))
        .attribute(AttrDef::new("bio", ValueType::Str, Kind::State, Cardinality::One).unindexed())
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
    mara: EntityId,
    varrow: EntityId,
    ashford: EntityId,
    carnath: EntityId,
}

fn fx() -> Fx {
    let mut w = World::in_memory(schema()).expect("world");
    let greg = w.create_entity("CHARACTER", Some("greg")).unwrap();
    let mara = w.create_entity("CHARACTER", Some("mara")).unwrap();
    let varrow = w.create_entity("PLACE", Some("varrow")).unwrap();
    let ashford = w.create_entity("PLACE", Some("ashford")).unwrap();
    let carnath = w.create_entity("PLACE", Some("carnath")).unwrap();
    Fx {
        w,
        greg,
        mara,
        varrow,
        ashford,
        carnath,
    }
}

fn t(x: i64) -> Option<Tick> {
    Some(Tick(x))
}

fn r(e: EntityId) -> Value {
    Value::Ref(e)
}

fn a(tick: i64, v: Value) -> Change {
    Change {
        tick: Tick(tick),
        op: Op::Assert,
        value: v,
        precision: Precision::Exact,
    }
}

fn x(tick: i64, v: Value) -> Change {
    Change {
        tick: Tick(tick),
        op: Op::Retract,
        value: v,
        precision: Precision::Exact,
    }
}

#[test]
fn empty_history_is_empty() {
    let f = fx();
    assert_eq!(f.w.history(f.greg, "located_in").unwrap(), vec![]);
}

#[test]
fn varrow_example_gives_three_changes_in_order() {
    let mut f = fx();
    let (greg, v) = (f.greg, f.varrow);
    f.w.assert(greg, "located_in", r(v), t(-300)).unwrap();
    f.w.retract(greg, "located_in", r(v), t(-250)).unwrap();
    f.w.assert(greg, "located_in", r(v), t(-200)).unwrap();
    assert_eq!(
        f.w.history(greg, "located_in").unwrap(),
        vec![a(-300, r(v)), x(-250, r(v)), a(-200, r(v))]
    );
}

#[test]
fn out_of_order_writes_come_back_in_tick_order_not_tx_order() {
    let mut f = fx();
    let (greg, v) = (f.greg, f.varrow);
    f.w.assert(greg, "located_in", r(v), t(-200)).unwrap();
    f.w.assert(greg, "located_in", r(v), t(-500))
        .unwrap_or_else(|e| panic!("{e}"));
    f.w.retract(greg, "located_in", r(v), t(-250)).unwrap();
    // @-500 assert is stored (nothing holds at -500); @-250 retract holds
    // at -250 via the -500 assert, so it is stored too.
    assert_eq!(
        f.w.history(greg, "located_in").unwrap(),
        vec![a(-500, r(v)), x(-250, r(v)), a(-200, r(v))]
    );
}

#[test]
fn many_valued_attribute_interleaves_values_by_tick() {
    let mut f = fx();
    let (greg, v, ash, car) = (f.greg, f.varrow, f.ashford, f.carnath);
    f.w.assert(greg, "owns", r(v), t(10)).unwrap();
    f.w.assert(greg, "owns", r(ash), t(5)).unwrap();
    f.w.assert(greg, "owns", r(car), t(20)).unwrap();
    f.w.retract(greg, "owns", r(v), t(15)).unwrap();
    f.w.retract(greg, "owns", r(ash), t(25)).unwrap();
    assert_eq!(
        f.w.history(greg, "owns").unwrap(),
        vec![
            a(5, r(ash)),
            a(10, r(v)),
            x(15, r(v)),
            a(20, r(car)),
            x(25, r(ash)),
        ]
    );
}

#[test]
fn same_tick_changes_of_different_values_are_ordered_by_value_encoding() {
    let mut f = fx();
    let greg = f.greg;
    // write in an order different from the value order
    for s in ["mmm", "aaa", "zzz", "aab"] {
        f.w.assert(greg, "alias", Value::Str(s.into()), t(7))
            .unwrap();
    }
    assert_eq!(
        f.w.history(greg, "alias").unwrap(),
        vec![
            a(7, Value::Str("aaa".into())),
            a(7, Value::Str("aab".into())),
            a(7, Value::Str("mmm".into())),
            a(7, Value::Str("zzz".into())),
        ]
    );
    // Ref values: encoding order is entity-ID order
    let (v, ash, car) = (f.varrow, f.ashford, f.carnath);
    for p in [car, v, ash] {
        f.w.assert(greg, "owns", r(p), t(1)).unwrap();
    }
    assert_eq!(
        f.w.history(greg, "owns").unwrap(),
        vec![a(1, r(v)), a(1, r(ash)), a(1, r(car))]
    );
    // an assert and a retract at the same tick, different values
    f.w.retract(greg, "owns", r(v), t(2)).unwrap();
    f.w.assert(greg, "owns", r(car), t(2))
        .unwrap_or_else(|e| panic!("{e}"));
    f.w.retract(greg, "owns", r(ash), t(2)).unwrap();
    assert_eq!(
        f.w.history(greg, "owns").unwrap()[3..].to_vec(),
        vec![x(2, r(v)), x(2, r(ash))]
    );
}

#[test]
fn numeric_values_at_one_tick_are_ordered_numerically() {
    let mut f = fx();
    let greg = f.greg;
    for n in [5i64, -3, 0, i64::MAX, i64::MIN] {
        f.w.assert(greg, "age", Value::I64(n), t(1)).unwrap();
    }
    let got: Vec<Value> =
        f.w.history(greg, "age")
            .unwrap()
            .into_iter()
            .map(|c| c.value)
            .collect();
    assert_eq!(
        got,
        vec![
            Value::I64(i64::MIN),
            Value::I64(-3),
            Value::I64(0),
            Value::I64(5),
            Value::I64(i64::MAX)
        ]
    );
}

#[test]
fn noop_and_failed_writes_do_not_appear() {
    let mut f = fx();
    let (greg, v, ash) = (f.greg, f.varrow, f.ashford);
    f.w.assert(greg, "located_in", r(v), t(10)).unwrap();
    // no-ops
    assert_eq!(
        f.w.assert(greg, "located_in", r(v), t(10)).unwrap().changed,
        0
    );
    assert_eq!(
        f.w.assert(greg, "located_in", r(v), t(15)).unwrap().changed,
        0
    );
    assert_eq!(
        f.w.retract(greg, "located_in", r(v), t(5)).unwrap().changed,
        0
    );
    assert_eq!(
        f.w.retract(greg, "located_in", r(ash), t(15))
            .unwrap()
            .changed,
        0
    );
    // failed writes
    assert!(f.w.retract(greg, "located_in", r(v), t(10)).is_err()); // same-tick conflict
    assert!(
        f.w.assert(greg, "located_in", Value::Bool(true), t(11))
            .is_err()
    );
    assert!(f.w.assert(greg, "nope", r(v), t(11)).is_err());
    assert_eq!(f.w.history(greg, "located_in").unwrap(), vec![a(10, r(v))]);
}

#[test]
fn an_out_of_order_retract_inside_a_past_stay_appears() {
    let mut f = fx();
    let (greg, v) = (f.greg, f.varrow);
    f.w.assert(greg, "located_in", r(v), t(-300)).unwrap();
    f.w.retract(greg, "located_in", r(v), t(-250)).unwrap();
    // stored: he was there at -280
    assert_eq!(
        f.w.retract(greg, "located_in", r(v), t(-280))
            .unwrap()
            .changed,
        1
    );
    assert_eq!(
        f.w.history(greg, "located_in").unwrap(),
        vec![a(-300, r(v)), x(-280, r(v)), x(-250, r(v))]
    );
}

#[test]
fn other_entities_and_attributes_do_not_leak() {
    let mut f = fx();
    let (greg, mara, v, ash) = (f.greg, f.mara, f.varrow, f.ashford);
    f.w.assert(greg, "located_in", r(v), t(1)).unwrap();
    f.w.assert(mara, "located_in", r(ash), t(2)).unwrap(); // adjacent ID
    f.w.assert(greg, "owns", r(ash), t(3)).unwrap(); // adjacent attribute
    f.w.assert(v, "alias", Value::Str("v".into()), t(4))
        .unwrap();
    assert_eq!(f.w.history(greg, "located_in").unwrap(), vec![a(1, r(v))]);
    assert_eq!(f.w.history(mara, "located_in").unwrap(), vec![a(2, r(ash))]);
    assert_eq!(f.w.history(greg, "owns").unwrap(), vec![a(3, r(ash))]);
    assert_eq!(f.w.history(greg, "alias").unwrap(), vec![]);
    // the same value under another entity's attribute of the same name
    assert_eq!(f.w.history(f.carnath, "located_in").unwrap(), vec![]);
}

#[test]
fn string_values_that_prefix_each_other_do_not_mix_and_stay_in_order() {
    let mut f = fx();
    let greg = f.greg;
    f.w.assert(greg, "alias", Value::Str("ab".into()), t(2))
        .unwrap();
    f.w.assert(greg, "alias", Value::Str("a".into()), t(3))
        .unwrap();
    f.w.assert(greg, "alias", Value::Str("abc".into()), t(1))
        .unwrap();
    f.w.retract(greg, "alias", Value::Str("a".into()), t(4))
        .unwrap();
    assert_eq!(
        f.w.history(greg, "alias").unwrap(),
        vec![
            a(1, Value::Str("abc".into())),
            a(2, Value::Str("ab".into())),
            a(3, Value::Str("a".into())),
            x(4, Value::Str("a".into())),
        ]
    );
}

#[test]
fn extreme_ticks_order_correctly_with_negatives_and_zero() {
    let mut f = fx();
    let (greg, v) = (f.greg, f.varrow);
    // out of order, retract/assert alternating on one value
    f.w.assert(greg, "located_in", r(v), t(0)).unwrap();
    f.w.assert(greg, "located_in", r(v), t(i64::MIN)).unwrap();
    f.w.retract(greg, "located_in", r(v), t(i64::MAX)).unwrap();
    f.w.retract(greg, "located_in", r(v), t(-1)).unwrap();
    f.w.assert(greg, "located_in", r(v), t(i64::MAX - 1))
        .unwrap();
    f.w.retract(greg, "located_in", r(v), t(1))
        .unwrap_or_else(|e| panic!("{e}"));
    let h = f.w.history(greg, "located_in").unwrap();
    let ticks: Vec<i64> = h.iter().map(|c| c.tick.0).collect();
    let mut sorted = ticks.clone();
    sorted.sort();
    assert_eq!(ticks, sorted);
    assert_eq!(ticks.first(), Some(&i64::MIN));
    assert_eq!(ticks.last(), Some(&i64::MAX));
    assert_eq!(h[0], a(i64::MIN, r(v)));
    assert_eq!(h[h.len() - 1], x(i64::MAX, r(v)));
    assert!(ticks.contains(&-1) && ticks.contains(&0));
}

#[test]
fn values_decode_back_exactly_for_every_type() {
    let mut f = fx();
    let greg = f.greg;
    let v = f.varrow;
    f.w.assert(greg, "located_in", r(v), t(1)).unwrap();
    f.w.assert(greg, "age", Value::I64(-42), t(1)).unwrap();
    f.w.assert(greg, "alive", Value::Bool(true), t(1)).unwrap();
    f.w.assert(greg, "alias", Value::Str("Gr\u{e9}g\0x".into()), t(1))
        .unwrap();
    f.w.assert(greg, "height", Value::F64(1.75), t(1)).unwrap();
    assert_eq!(f.w.history(greg, "located_in").unwrap()[0].value, r(v));
    assert_eq!(f.w.history(greg, "age").unwrap()[0].value, Value::I64(-42));
    assert_eq!(
        f.w.history(greg, "alive").unwrap()[0].value,
        Value::Bool(true)
    );
    assert_eq!(
        f.w.history(greg, "alias").unwrap()[0].value,
        Value::Str("Gr\u{e9}g\0x".into())
    );
    assert_eq!(
        f.w.history(greg, "height").unwrap()[0].value,
        Value::F64(1.75)
    );
}

#[test]
fn negative_zero_comes_back_as_positive_zero() {
    let mut f = fx();
    let greg = f.greg;
    f.w.assert(greg, "height", Value::F64(-0.0), t(1)).unwrap();
    let h = f.w.history(greg, "height").unwrap();
    assert_eq!(h.len(), 1);
    match h[0].value.clone() {
        Value::F64(z) => {
            assert_eq!(z, 0.0);
            assert!(z.is_sign_positive(), "-0.0 must be normalized to 0.0");
        }
        other => panic!("expected F64, got {other:?}"),
    }
}

#[test]
fn long_unindexed_string_round_trips() {
    let mut f = fx();
    let greg = f.greg;
    let bio = "long biography \u{1F409} ".repeat(500);
    assert!(bio.len() > 256);
    f.w.assert(greg, "bio", Value::Str(bio.clone()), t(1))
        .unwrap();
    f.w.retract(greg, "bio", Value::Str(bio.clone()), t(2))
        .unwrap();
    assert_eq!(
        f.w.history(greg, "bio").unwrap(),
        vec![a(1, Value::Str(bio.clone())), x(2, Value::Str(bio))]
    );
}

#[test]
fn at_canon_history_excludes_later_tx_changes_including_a_prequel() {
    let mut f = fx();
    let (greg, v) = (f.greg, f.varrow);
    f.w.assert(greg, "located_in", r(v), t(-300)).unwrap();
    let c1 = f.w.latest_tx();
    f.w.retract(greg, "located_in", r(v), t(-250)).unwrap();
    let c2 = f.w.latest_tx();
    f.w.assert(greg, "located_in", r(v), t(-500)).unwrap(); // prequel: tx later, tick earlier
    let c3 = f.w.latest_tx();
    assert_eq!(
        f.w.at_canon(c1).history(greg, "located_in").unwrap(),
        vec![a(-300, r(v))]
    );
    assert_eq!(
        f.w.at_canon(c2).history(greg, "located_in").unwrap(),
        vec![a(-300, r(v)), x(-250, r(v))]
    );
    // the prequel assert @-500 while the value was absent at -500 is stored
    let full = vec![a(-500, r(v)), a(-300, r(v)), x(-250, r(v))];
    assert_eq!(f.w.at_canon(c3).history(greg, "located_in").unwrap(), full);
    assert_eq!(f.w.history(greg, "located_in").unwrap(), full);
}

#[test]
fn at_canon_zero_history_is_empty_and_beyond_latest_is_latest() {
    let mut f = fx();
    let (greg, v) = (f.greg, f.varrow);
    f.w.assert(greg, "located_in", r(v), t(1)).unwrap();
    // greg is not defined at canon 0
    assert_eq!(
        f.w.at_canon(TxId(0)).history(greg, "located_in"),
        Err(Error::UnknownEntity(greg))
    );
    assert_eq!(
        f.w.at_canon(TxId(u64::MAX))
            .history(greg, "located_in")
            .unwrap(),
        vec![a(1, r(v))]
    );
}

#[test]
fn history_of_a_txn_is_visible_only_after_commit() {
    let mut f = fx();
    let (greg, v, ash) = (f.greg, f.varrow, f.ashford);
    let before = f.w.latest_tx();
    let mut txn = f.w.begin();
    txn.assert(greg, "located_in", r(v), t(1)).unwrap();
    txn.assert(greg, "owns", r(ash), t(2)).unwrap();
    txn.commit().unwrap();
    assert_eq!(
        f.w.at_canon(before).history(greg, "located_in").unwrap(),
        vec![]
    );
    assert_eq!(f.w.at_canon(before).history(greg, "owns").unwrap(), vec![]);
    assert_eq!(f.w.history(greg, "located_in").unwrap(), vec![a(1, r(v))]);
    assert_eq!(f.w.history(greg, "owns").unwrap(), vec![a(2, r(ash))]);
}

#[test]
fn event_attribute_history_shows_its_asserts() {
    let mut f = fx();
    let (greg, v, ash) = (f.greg, f.varrow, f.ashford);
    f.w.assert(greg, "bought", r(ash), t(-208)).unwrap();
    f.w.assert(greg, "bought", r(v), t(-300)).unwrap();
    assert_eq!(
        f.w.history(greg, "bought").unwrap(),
        vec![a(-300, r(v)), a(-208, r(ash))]
    );
}

#[test]
fn unknown_attribute_is_an_error() {
    let f = fx();
    assert_eq!(
        f.w.history(f.greg, "nope"),
        Err(Error::UnknownAttribute("nope".to_string()))
    );
    assert_eq!(
        f.w.at_canon(TxId(0)).history(f.greg, "nope"),
        Err(Error::UnknownAttribute("nope".to_string()))
    );
}

#[test]
fn undefined_entity_is_an_error() {
    let f = fx();
    let ghost = EntityId::new(f.greg.node_type(), 99).unwrap();
    assert_eq!(f.w.history(ghost, "owns"), Err(Error::UnknownEntity(ghost)));
    assert_eq!(
        f.w.at_canon(TxId(0)).history(ghost, "owns"),
        Err(Error::UnknownEntity(ghost))
    );
}

#[test]
fn history_agrees_with_as_of_at_every_change_point() {
    let mut f = fx();
    let (greg, v, ash) = (f.greg, f.varrow, f.ashford);
    f.w.assert(greg, "owns", r(v), t(3)).unwrap();
    f.w.assert(greg, "owns", r(ash), t(1)).unwrap();
    f.w.retract(greg, "owns", r(v), t(9)).unwrap();
    f.w.assert(greg, "owns", r(v), t(12)).unwrap();
    for c in f.w.history(greg, "owns").unwrap() {
        let holds = f.w.as_of_value(greg, "owns", &c.value, c.tick).unwrap();
        assert_eq!(holds, c.op == Op::Assert, "at {:?}", c);
    }
}

#[test]
fn at_canon_before_an_entity_existed_is_unknown_entity() {
    let mut f = fx();
    let old = f.w.latest_tx();
    let late = f.w.create_entity("CHARACTER", Some("late")).unwrap();
    f.w.assert(late, "located_in", r(f.varrow), t(5)).unwrap();
    assert_eq!(
        f.w.at_canon(old).history(late, "located_in"),
        Err(Error::UnknownEntity(late))
    );
}
