//! A commit caps reports; complete doctor diagnostics retain their existing scope.
use kala_core::*;

fn world(cardinality: Cardinality) -> (World, EntityId) {
    let mut w = World::in_memory(Schema::new().node_type("CHARACTER").attribute(AttrDef::new(
        "rank",
        ValueType::I64,
        Kind::State,
        cardinality,
    )))
    .unwrap();
    let e = w.create_entity("CHARACTER", Some("e")).unwrap();
    (w, e)
}

#[test]
fn commit_warning_lists_are_capped_without_changing_doctor() {
    let (mut w, e) = world(Cardinality::One);
    let mut tx = w.begin();
    for i in 0..17 {
        tx.assert(e, "rank", Value::I64(i), Some(Tick(0))).unwrap();
    }
    let result = tx.commit().unwrap();
    assert_eq!(result.warnings.introduced.len(), 100);
    let overlaps = w.overlaps("rank").unwrap();
    assert_eq!(overlaps.len(), 136);
    assert_eq!(result.warnings.introduced, overlaps[..100]);
}

#[test]
fn commit_warning_cap_counts_introduced_resolved_and_schema_changes() {
    let (mut w, e) = world(Cardinality::Many);
    let mut tx = w.begin();
    for i in 0..17 {
        tx.assert(e, "rank", Value::I64(i), Some(Tick(0))).unwrap();
    }
    assert!(tx.commit().unwrap().warnings.introduced.is_empty());
    let mut tx = w.begin();
    tx.set_cardinality("rank", Cardinality::One).unwrap();
    let result = tx.commit().unwrap();
    assert_eq!(result.warnings.introduced.len(), MAX_COMMIT_WARNINGS);
    assert_eq!(result.warnings.introduced_omitted, 36);
    assert_eq!(result.warnings.resolved_omitted, 0);
    let all = w.overlaps("rank").unwrap();
    let mut tx = w.begin();
    for i in 0..17 {
        tx.retract(e, "rank", Value::I64(i), Some(Tick(10)))
            .unwrap();
    }
    let result = tx.commit().unwrap();
    assert_eq!(result.warnings.introduced.len(), MAX_COMMIT_WARNINGS);
    assert_eq!(result.warnings.resolved, all[..MAX_COMMIT_WARNINGS]);
    assert_eq!(result.warnings.introduced_omitted, 36);
    assert_eq!(result.warnings.resolved_omitted, 36);
    let mut tx = w.begin();
    tx.set_cardinality("rank", Cardinality::Many).unwrap();
    let result = tx.commit().unwrap();
    assert_eq!(result.warnings.resolved.len(), MAX_COMMIT_WARNINGS);
    assert_eq!(result.warnings.resolved_omitted, 36);
    assert!(result.warnings.introduced.is_empty());
}
