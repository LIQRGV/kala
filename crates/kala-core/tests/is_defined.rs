//! Slice 7, item 00 (f), L5: `View::is_defined(&self, id: EntityId) -> bool`
//! and `World::is_defined(&self, id: EntityId) -> bool` (latest canon): true
//! iff the ID's DEFINE is in a tx <= the view's canon; entity and edge IDs;
//! false for IDs never defined.

use kala_core::*;

fn schema() -> Schema {
    Schema::new().node_type("CHARACTER").attribute(AttrDef::new(
        "knows",
        ValueType::Ref,
        Kind::State,
        Cardinality::Many,
    ))
}

struct F {
    w: World,
    a: EntityId,
    b: EntityId,
    edge: EntityId,
    tx_a: TxId,
    tx_b: TxId,
    tx_edge: TxId,
}

fn fixture() -> F {
    let mut w = World::in_memory(schema()).unwrap();
    let a = w.create_entity("CHARACTER", Some("a")).unwrap();
    let tx_a = w.latest_tx();
    let b = w.create_entity("CHARACTER", None).unwrap();
    let tx_b = w.latest_tx();
    w.assert(a, "knows", Value::Ref(b), Some(Tick(0))).unwrap();
    let tx_edge = w.latest_tx();
    let edge = EntityId::from(w.edge_id(a, "knows", b).expect("edge interned"));
    assert!(tx_a < tx_b && tx_b < tx_edge);
    F {
        w,
        a,
        b,
        edge,
        tx_a,
        tx_b,
        tx_edge,
    }
}

/// An ID that exists in another, bigger world but never in `fixture()`'s.
fn foreign() -> (EntityId, EntityId) {
    let mut w = World::in_memory(schema()).unwrap();
    let mut last = None;
    for _ in 0..9 {
        last = Some(w.create_entity("CHARACTER", None).unwrap());
    }
    let x = last.unwrap();
    let mut y = x;
    for _ in 0..4 {
        y = w.create_entity("CHARACTER", None).unwrap();
        w.assert(x, "knows", Value::Ref(y), Some(Tick(0))).unwrap();
    }
    (y, EntityId::from(w.edge_id(x, "knows", y).unwrap()))
}

#[test]
fn defined_ids_are_defined_at_the_latest_canon() {
    let f = fixture();
    for id in [f.a, f.b, f.edge] {
        assert!(f.w.is_defined(id));
        assert!(f.w.at_canon(f.w.latest_tx()).is_defined(id));
    }
}

#[test]
fn is_defined_follows_the_canon() {
    let f = fixture();
    let cases = [
        (TxId(0), [false, false, false]),
        (f.tx_a, [true, false, false]),
        (f.tx_b, [true, true, false]),
        (f.tx_edge, [true, true, true]),
    ];
    for (canon, want) in cases {
        for v in [f.w.at_canon(canon), f.w.at_canon_main(canon)] {
            let got = [f.a, f.b, f.edge].map(|id| v.is_defined(id));
            assert_eq!(got, want, "canon {canon:?}");
        }
    }
}

#[test]
fn ids_never_defined_are_not_defined() {
    let f = fixture();
    let (far_entity, far_edge) = foreign();
    // Precondition: the foreign IDs really are ones the fixture never defines.
    assert_ne!(far_edge, f.edge);
    assert!(![f.a, f.b].contains(&far_entity));
    for id in [far_entity, far_edge] {
        assert!(!f.w.is_defined(id));
        assert!(!f.w.at_canon(f.w.latest_tx()).is_defined(id));
    }
}
