//! Same-tick diagnostics use bounded native reads even after a long author history.
use kala_core::*;

fn world() -> (World, EntityId) {
    let mut w = World::in_memory(Schema::new().node_type("CHARACTER").attribute(AttrDef::new(
        "v",
        ValueType::I64,
        Kind::State,
        Cardinality::Many,
    )))
    .unwrap();
    let e = w.create_entity("CHARACTER", Some("e")).unwrap();
    (w, e)
}

#[test]
fn conflict_entry_lookup_skips_a_large_unrelated_later_transaction() {
    let (mut w, e) = world();
    let original = w
        .assert(e, "v", Value::I64(0), Some(Tick(0)))
        .unwrap()
        .entries[0];
    let mut tx = w.begin();
    for i in 1..=100_000 {
        tx.assert(e, "v", Value::I64(i), Some(Tick(1))).unwrap();
    }
    tx.commit_quiet().unwrap();
    let tx = w.begin();
    let meter = WorkMeter::bounded(128, None);
    let found = tx
        .entry_at_tick(e, "v", &Value::I64(0), Tick(0), &meter)
        .unwrap();
    assert_eq!(
        found,
        Some(TickOccupant::Committed {
            entry: original,
            op: Op::Assert
        })
    );
    assert!(
        meter.keys() > 0 && meter.keys() <= 128,
        "diagnostic lookup must charge its bounded work: {}",
        meter.keys()
    );
}

#[test]
fn conflict_entry_lookup_stops_when_its_owning_transaction_exceeds_the_budget() {
    let (mut w, e) = world();
    let mut tx = w.begin();
    for i in 0..1024 {
        tx.assert(e, "v", Value::I64(i), Some(Tick(0))).unwrap();
    }
    tx.commit_quiet().unwrap();
    let tx = w.begin();
    let meter = WorkMeter::bounded(8, None);
    let result = tx.entry_at_tick(e, "v", &Value::I64(512), Tick(0), &meter);
    assert!(
        matches!(result, Err(Error::WorkInterrupted { .. })),
        "{result:?}"
    );
    assert!(
        meter.keys() <= 9,
        "work continued after the first budget-crossing entry"
    );
}
