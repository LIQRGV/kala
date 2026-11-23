//! Facts and reported schema actions have independent counts.
use kala_core::*;

fn counts(result: CommitResult, changed: usize, declared: usize) {
    assert_eq!(result.changed, changed, "fact count: {result:?}");
    assert_eq!(result.declared, declared, "schema action count: {result:?}");
    assert_eq!(result.entries.len(), changed);
    assert_eq!(result.schema.len(), declared);
}

#[test]
fn every_schema_action_is_declared_without_counting_as_a_fact() {
    let mut world = World::in_memory(Schema::new().node_type("CHARACTER")).unwrap();
    let mut tx = world.begin();
    tx.declare_attribute(AttrDef::new(
        "name",
        ValueType::Str,
        Kind::State,
        Cardinality::One,
    ))
    .unwrap();
    counts(tx.commit().unwrap(), 0, 1);
    let mut tx = world.begin();
    tx.declare_node_type("PLACE").unwrap();
    counts(tx.commit().unwrap(), 0, 1);
    let mut tx = world.begin();
    tx.infer_node_type("ITEM").unwrap();
    counts(tx.commit().unwrap(), 0, 1);
    let mut tx = world.begin();
    tx.declare_node_type("ITEM").unwrap();
    counts(tx.commit().unwrap(), 0, 1);
    let mut tx = world.begin();
    tx.create_entity("CHARACTER", Some("ann")).unwrap();
    counts(tx.commit().unwrap(), 0, 0);
    let e = world.entity_by_handle("ann").unwrap();
    let mut tx = world.begin();
    tx.infer_attribute("age", &Value::I64(42)).unwrap();
    tx.assert(e, "age", Value::I64(42), Some(Tick(i64::MIN)))
        .unwrap();
    counts(tx.commit().unwrap(), 1, 1);
    let mut tx = world.begin();
    tx.declare_attribute(AttrDef::new(
        "age",
        ValueType::I64,
        Kind::State,
        Cardinality::One,
    ))
    .unwrap();
    counts(tx.commit().unwrap(), 0, 1);
    let mut tx = world.begin();
    tx.rename_attribute("name", "title").unwrap();
    counts(tx.commit().unwrap(), 0, 1);
    let mut tx = world.begin();
    tx.set_cardinality("title", Cardinality::Many).unwrap();
    counts(tx.commit().unwrap(), 0, 1);
    let mut tx = world.begin();
    tx.deprecate_attribute("title").unwrap();
    counts(tx.commit().unwrap(), 0, 1);
    let mut tx = world.begin();
    tx.world_setting("schema", "strict").unwrap();
    counts(tx.commit().unwrap(), 0, 1);
    let before = world.latest_tx();
    let mut tx = world.begin();
    tx.declare_node_type("PLACE").unwrap();
    tx.declare_attribute(AttrDef::new(
        "age",
        ValueType::I64,
        Kind::State,
        Cardinality::One,
    ))
    .unwrap();
    tx.rename_attribute("age", "age").unwrap();
    tx.set_cardinality("title", Cardinality::Many).unwrap();
    tx.deprecate_attribute("title").unwrap();
    tx.world_setting("schema", "strict").unwrap();
    tx.assert(e, "age", Value::I64(42), Some(Tick(i64::MAX)))
        .unwrap();
    let no_op = tx.commit().unwrap();
    assert_eq!(no_op.tx, before);
    counts(no_op, 0, 0);
}

#[test]
fn migration_and_corrections_count_stored_fact_entries_only() {
    let mut world = World::in_memory(Schema::new().node_type("CHARACTER")).unwrap();
    let e = world.create_entity("CHARACTER", None).unwrap();
    let mut tx = world.begin();
    tx.infer_attribute("age", &Value::I64(42)).unwrap();
    tx.assert(e, "age", Value::I64(42), Some(Tick(-20)))
        .unwrap();
    tx.retract(e, "age", Value::I64(42), Some(Tick(0))).unwrap();
    tx.commit().unwrap();
    let mut tx = world.begin();
    let migrated = tx
        .migrate_attribute(
            "age",
            MigrationOptions {
                value_type: Some(ValueType::Str),
                ..MigrationOptions::default()
            },
        )
        .unwrap();
    assert_eq!(migrated.change_points, 2);
    let result = tx.commit().unwrap();
    let entry = result.entries[0];
    counts(result, 2, 4);
    let mut tx = world.begin();
    tx.correct(
        entry,
        Correction::Replace {
            value: None,
            tick: Some(Tick(-30)),
            op: None,
            precision: None,
        },
    )
    .unwrap();
    counts(tx.commit().unwrap(), 2, 0);
    let mut tx = world.begin();
    tx.correct(entry, Correction::Delete).unwrap();
    counts(tx.commit().unwrap(), 1, 0);
}

#[test]
fn edge_interning_does_not_add_to_either_count() {
    let mut world = World::in_memory(Schema::new().node_type("CHARACTER")).unwrap();
    let a = world.create_entity("CHARACTER", None).unwrap();
    let b = world.create_entity("CHARACTER", None).unwrap();
    let mut tx = world.begin();
    tx.infer_attribute("ally", &Value::Ref(b)).unwrap();
    tx.assert(a, "ally", Value::Ref(b), Some(Tick(0))).unwrap();
    let result = tx.commit().unwrap();
    assert_eq!(result.edges.len(), 1);
    counts(result, 1, 1);
}
