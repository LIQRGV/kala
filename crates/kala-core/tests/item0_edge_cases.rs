//! Slice 6, item 0: edge cases found after the implementation (CONFIRM step).

use kala_core::*;
use std::cmp::Ordering;

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
        .attribute(a(
            "disguised",
            ValueType::Bool,
            Kind::State,
            Cardinality::One,
        ))
        .attribute(a("rare", ValueType::I64, Kind::State, Cardinality::Many))
        .attribute(a("unused", ValueType::I64, Kind::State, Cardinality::Many))
}

fn tk(x: i64) -> Option<Tick> {
    Some(Tick(x))
}

fn r(e: EntityId) -> Value {
    Value::Ref(e)
}

#[test]
fn attr_scan_at_older_canon_ignores_entity_defined_later() {
    let mut w = World::in_memory(schema()).unwrap();
    let greg = w.create_entity("CHARACTER", Some("greg")).unwrap();
    let varrow = w.create_entity("PLACE", Some("varrow")).unwrap();
    w.assert(greg, "located_in", r(varrow), tk(0)).unwrap();
    let old = w.latest_tx();

    let late = w.create_entity("CHARACTER", Some("late")).unwrap();
    w.assert(late, "located_in", r(varrow), tk(0)).unwrap();

    let at_old = w.at_canon(old).attr_scan("located_in", Tick(0)).unwrap();
    assert_eq!(at_old, vec![(greg, r(varrow))]);
    let now = w.attr_scan("located_in", Tick(0)).unwrap();
    assert_eq!(now.len(), 2);
    assert!(now.contains(&(late, r(varrow))));
}

#[test]
fn attr_scan_inverse_name_self_pair_appears_once() {
    let mut w = World::in_memory(schema()).unwrap();
    let greg = w.create_entity("CHARACTER", Some("greg")).unwrap();
    w.assert(greg, "parent_of", r(greg), tk(0)).unwrap();
    assert_eq!(
        w.attr_scan("child_of", Tick(0)).unwrap(),
        vec![(greg, r(greg))]
    );
    assert_eq!(
        w.attr_scan("parent_of", Tick(0)).unwrap(),
        vec![(greg, r(greg))]
    );
}

#[test]
fn attr_scan_on_event_attribute_holds_every_asserted_value_up_to_t() {
    let mut w = World::in_memory(schema()).unwrap();
    let a = w.create_entity("CHARACTER", Some("a")).unwrap();
    let p1 = w.create_entity("PLACE", Some("p1")).unwrap();
    let p2 = w.create_entity("PLACE", Some("p2")).unwrap();
    w.assert(a, "bought", r(p1), tk(-10)).unwrap();
    w.assert(a, "bought", r(p2), tk(5)).unwrap();
    assert!(w.attr_scan("bought", Tick(-11)).unwrap().is_empty());
    assert_eq!(w.attr_scan("bought", Tick(-10)).unwrap(), vec![(a, r(p1))]);
    assert_eq!(w.attr_scan("bought", Tick(4)).unwrap(), vec![(a, r(p1))]);
    assert_eq!(
        w.attr_scan("bought", Tick(5)).unwrap(),
        vec![(a, r(p1)), (a, r(p2))]
    );
    assert_eq!(w.attr_scan("bought", Tick(i64::MAX)).unwrap().len(), 2);
}

#[test]
fn nan_ordering_pin_as_documented() {
    let nan = Value::F64(f64::NAN);
    let neg_nan = Value::F64(f64::from_bits(0xfff8_0000_0000_0001));
    // all NaNs equal
    assert_eq!(nan.cmp(&neg_nan), Ordering::Equal);
    assert!(nan == neg_nan);
    // after +inf (and everything numeric), before any Str
    assert_eq!(nan.cmp(&Value::F64(f64::INFINITY)), Ordering::Greater);
    assert_eq!(nan.cmp(&Value::F64(f64::MAX)), Ordering::Greater);
    assert_eq!(nan.cmp(&Value::Str(String::new())), Ordering::Less);
    assert_eq!(nan.cmp(&Value::Str("zzz".into())), Ordering::Less);
}

#[test]
fn attr_scan_world_equals_latest_canon_view() {
    let mut w = World::in_memory(schema()).unwrap();
    let c: Vec<_> = (0..5)
        .map(|i| {
            w.create_entity("CHARACTER", Some(&format!("c{i}")))
                .unwrap()
        })
        .collect();
    let p = w.create_entity("PLACE", Some("p")).unwrap();
    for (i, e) in c.iter().enumerate() {
        w.assert(*e, "located_in", r(p), tk(i as i64 * 10)).unwrap();
        w.assert(*e, "parent_of", r(c[(i + 1) % 5]), tk(i as i64))
            .unwrap();
    }
    w.retract(c[2], "located_in", r(p), tk(25)).unwrap();
    for attr in ["located_in", "parent_of", "child_of"] {
        for t in [i64::MIN, -1, 0, 5, 20, 25, 26, 40, i64::MAX] {
            let a = w.attr_scan(attr, Tick(t)).unwrap();
            let b = w.at_canon(w.latest_tx()).attr_scan(attr, Tick(t)).unwrap();
            assert_eq!(a, b, "{attr} @{t}");
            let mut sorted = a.clone();
            sorted.sort();
            sorted.dedup();
            assert_eq!(a, sorted, "sorted and unique");
        }
    }
}

#[test]
fn attr_stats_and_scan_for_attribute_used_only_on_edge_entities() {
    let mut w = World::in_memory(schema()).unwrap();
    let greg = w.create_entity("CHARACTER", Some("greg")).unwrap();
    let varrow = w.create_entity("PLACE", Some("varrow")).unwrap();
    let ash = w.create_entity("PLACE", Some("ash")).unwrap();
    let e1 = EntityId::from(
        w.assert(greg, "located_in", r(varrow), tk(0))
            .unwrap()
            .edges[0],
    );
    let e2 = EntityId::from(w.assert(greg, "located_in", r(ash), tk(0)).unwrap().edges[0]);
    assert_eq!(
        w.attr_stats("disguised").unwrap(),
        AttrStats {
            entities: 0,
            values: 0,
            pairs: 0
        }
    );
    w.assert(e1, "disguised", Value::Bool(true), tk(0)).unwrap();
    w.assert(e2, "disguised", Value::Bool(true), tk(0)).unwrap();
    w.assert(e1, "disguised", Value::Bool(true), tk(0)).unwrap(); // no-op
    assert_eq!(
        w.attr_stats("disguised").unwrap(),
        AttrStats {
            entities: 2,
            values: 1,
            pairs: 2
        }
    );
    let mut want = vec![(e1, Value::Bool(true)), (e2, Value::Bool(true))];
    want.sort();
    assert_eq!(w.attr_scan("disguised", Tick(0)).unwrap(), want);
    // never-used attribute
    assert_eq!(
        w.attr_stats("unused").unwrap(),
        AttrStats {
            entities: 0,
            values: 0,
            pairs: 0
        }
    );
    assert!(w.attr_scan("unused", Tick(0)).unwrap().is_empty());
}

#[test]
fn attr_scan_large_world_sparse_attribute() {
    let mut w = World::in_memory(schema()).unwrap();
    let mut ids = Vec::new();
    let mut tx = w.begin();
    for _ in 0..2000 {
        ids.push(tx.create_entity("CHARACTER", None).unwrap());
    }
    for (i, k) in [(7usize, 1i64), (999, 2), (1999, 3)] {
        tx.assert(ids[i], "rare", Value::I64(k), tk(0)).unwrap();
    }
    tx.commit().unwrap();
    let got = w.attr_scan("rare", Tick(0)).unwrap();
    assert_eq!(
        got,
        vec![
            (ids[7], Value::I64(1)),
            (ids[999], Value::I64(2)),
            (ids[1999], Value::I64(3))
        ]
    );
    assert!(w.attr_scan("rare", Tick(-1)).unwrap().is_empty());
    assert_eq!(
        w.attr_stats("rare").unwrap(),
        AttrStats {
            entities: 3,
            values: 3,
            pairs: 3
        }
    );
}
