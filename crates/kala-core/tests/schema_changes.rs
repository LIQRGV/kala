//! Schema revisions are author-time log entries; names resolve at latest canon.

use kala_core::*;

fn schema() -> Schema {
    Schema::new()
        .node_type("CHARACTER")
        .node_type("PLACE")
        .attribute(AttrDef::new(
            "name",
            ValueType::Str,
            Kind::State,
            Cardinality::One,
        ))
        .attribute(AttrDef::new(
            "located_in",
            ValueType::Ref,
            Kind::State,
            Cardinality::One,
        ))
}

#[test]
fn unknown_attribute_is_inferred_and_reported() {
    let mut world = World::in_memory(schema()).unwrap();
    let edwin = world.create_entity("CHARACTER", Some("edwin")).unwrap();
    let mut tx = world.begin();
    let inferred = tx.infer_attribute("age", &Value::I64(42));
    assert!(
        inferred.is_ok(),
        "an unknown attribute must be inferred from its first value: {inferred:?}"
    );
    tx.assert(edwin, "age", Value::I64(42), Some(Tick(0)))
        .unwrap();
    let committed = tx.commit().unwrap();
    assert_eq!(committed.schema.len(), 1, "{committed:?}");
    let declaration = format!("{:?}", committed.schema[0]);
    assert!(declaration.contains("age"), "{declaration}");
    assert!(declaration.contains("I64"), "{declaration}");
    assert!(declaration.contains("inferred: true"), "{declaration}");
    assert_eq!(
        world.as_of(edwin, "age", Tick(0)).unwrap(),
        [Value::I64(42)]
    );
}

fn attr(name: &str, value_type: ValueType) -> AttrDef {
    AttrDef::new(name, value_type, Kind::State, Cardinality::One)
}

#[test]
fn inference_uses_value_types_and_directed_many_for_refs() {
    let mut world = World::in_memory(schema()).unwrap();
    let edwin = world.create_entity("CHARACTER", Some("edwin")).unwrap();
    let tomas = world.create_entity("CHARACTER", Some("tomas")).unwrap();
    for (name, value, ty) in [
        ("age", Value::I64(42), ValueType::I64),
        ("height", Value::F64(1.8), ValueType::F64),
        ("alive", Value::Bool(true), ValueType::Bool),
        ("title", Value::Str("king".into()), ValueType::Str),
        ("mentor_of", Value::Ref(tomas), ValueType::Ref),
    ] {
        let mut tx = world.begin();
        tx.infer_attribute(name, &value).unwrap();
        tx.assert(edwin, name, value, Some(Tick(-300))).unwrap();
        assert_eq!(tx.commit().unwrap().schema.len(), 1);
        let def = world.attribute(name).unwrap();
        assert_eq!(def.value_type, ty);
        assert_eq!(def.kind, Kind::State);
        assert!(def.indexed && def.inferred && !def.deprecated);
        assert_eq!(
            def.cardinality,
            if ty == ValueType::Ref {
                Cardinality::Many
            } else {
                Cardinality::One
            }
        );
        assert_eq!(
            def.direction,
            (ty == ValueType::Ref).then_some(RefDirection::Directed)
        );
    }
    assert!(world.edge_id(edwin, "mentor_of", tomas).is_some());
}

#[test]
fn inference_is_ordered_and_a_wrong_type_aborts_the_whole_transaction() {
    let mut world = World::in_memory(schema()).unwrap();
    let edwin = world.create_entity("CHARACTER", None).unwrap();
    let before = world.latest_tx();
    let mut tx = world.begin();
    tx.infer_attribute("age", &Value::I64(42)).unwrap();
    tx.assert(edwin, "age", Value::I64(42), Some(Tick(0)))
        .unwrap();
    tx.infer_attribute("age", &Value::Str("old".into()))
        .unwrap();
    assert!(matches!(
        tx.assert(edwin, "age", Value::Str("old".into()), Some(Tick(1))),
        Err(Error::WrongValueType { .. })
    ));
    assert!(tx.commit().is_err());
    assert!(world.attribute("age").is_none());
    assert_eq!(world.latest_tx(), before);
}

#[test]
fn dropped_schema_changes_leave_names_ids_and_settings_available() {
    let mut world = World::in_memory(schema()).unwrap();
    let before = world.latest_tx();
    {
        let mut tx = world.begin();
        tx.declare_attribute(attr("age", ValueType::I64)).unwrap();
        tx.rename_attribute("name", "title").unwrap();
        tx.deprecate_attribute("located_in").unwrap();
        tx.infer_node_type("FACTION").unwrap();
        tx.create_entity("FACTION", Some("red_hand")).unwrap();
        tx.world_setting("schema", "strict").unwrap();
    }
    assert_eq!(world.latest_tx(), before);
    assert!(world.attribute("age").is_none());
    assert!(world.attribute("title").is_none());
    assert!(world.attribute("name").is_some());
    assert!(!world.attribute("located_in").unwrap().deprecated);
    let mut tx = world.begin();
    tx.infer_attribute("age", &Value::I64(42)).unwrap();
    tx.infer_node_type("FACTION").unwrap();
    let faction = tx.create_entity("FACTION", Some("red_hand")).unwrap();
    assert_eq!(faction.seq(), 1);
    tx.commit().unwrap();
}

#[test]
fn strict_setting_applies_to_the_next_op_and_explicit_declaration_works() {
    let mut world = World::in_memory(schema()).unwrap();
    let mut tx = world.begin();
    tx.world_setting("schema", "strict").unwrap();
    let error = tx
        .infer_attribute("height", &Value::I64(180))
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("height") && error.contains("[:attribute :height :i64]"),
        "{error}"
    );
    assert!(tx.commit().is_err());
    let mut tx = world.begin();
    tx.world_setting("schema", "strict").unwrap();
    tx.commit().unwrap();
    let mut tx = world.begin();
    let error = tx.infer_node_type("GUILD").unwrap_err().to_string();
    assert!(
        error.contains("GUILD") && error.contains("[:node-type :GUILD]"),
        "{error}"
    );
    drop(tx);
    let mut tx = world.begin();
    tx.declare_attribute(attr("height", ValueType::I64))
        .unwrap();
    tx.declare_node_type("GUILD").unwrap();
    let g = tx.create_entity("GUILD", Some("g1")).unwrap();
    tx.assert(g, "height", Value::I64(180), Some(Tick(0)))
        .unwrap();
    tx.world_setting("schema", "infer").unwrap();
    tx.infer_attribute("age", &Value::I64(42)).unwrap();
    tx.commit().unwrap();
    assert_eq!(
        world.as_of(g, "height", Tick(0)).unwrap(),
        [Value::I64(180)]
    );
}

#[test]
fn inferred_origins_become_declared_and_repeated_declarations_are_noops() {
    let mut world = World::in_memory(schema()).unwrap();
    let mut tx = world.begin();
    tx.infer_attribute("age", &Value::I64(42)).unwrap();
    tx.infer_node_type("FACTION").unwrap();
    let inferred_tx = tx.commit().unwrap().tx;
    let mut tx = world.begin();
    tx.declare_attribute(attr("age", ValueType::I64)).unwrap();
    tx.declare_node_type("FACTION").unwrap();
    let declared = tx.commit().unwrap();
    assert_eq!(declared.schema.len(), 2);
    assert!(!world.attribute("age").unwrap().inferred);
    assert!(
        world
            .at_canon(inferred_tx)
            .attribute("age")
            .unwrap()
            .inferred
    );
    let mut tx = world.begin();
    tx.declare_attribute(attr("age", ValueType::I64)).unwrap();
    tx.declare_node_type("FACTION").unwrap();
    let same = tx.commit().unwrap();
    assert!(same.schema.is_empty());
    assert_eq!(same.tx, declared.tx);
}

#[test]
fn rename_is_canon_independent_and_old_name_can_be_reused_in_same_transaction() {
    let mut world = World::in_memory(schema()).unwrap();
    let edwin = world.create_entity("CHARACTER", None).unwrap();
    world
        .assert(
            edwin,
            "name",
            Value::Str("Edwin".into()),
            Some(Tick(i64::MIN)),
        )
        .unwrap();
    let before = world.latest_tx();
    let mut tx = world.begin();
    tx.rename_attribute("name", "title").unwrap();
    tx.declare_attribute(attr("name", ValueType::I64)).unwrap();
    tx.assert(edwin, "name", Value::I64(42), Some(Tick(i64::MAX)))
        .unwrap();
    tx.commit().unwrap();
    for canon in [before, world.latest_tx()] {
        assert_eq!(
            world
                .at_canon(canon)
                .as_of(edwin, "title", Tick(0))
                .unwrap(),
            [Value::Str("Edwin".into())]
        );
        assert_eq!(
            world.at_canon(canon).attribute("title").unwrap().name,
            "title"
        );
    }
    let error = world
        .at_canon(before)
        .as_of(edwin, "name", Tick(0))
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("name")
            && error.contains(&format!("tx#{}", before.0))
            && error.contains("not defined"),
        "{error}"
    );
    assert_eq!(
        world.as_of(edwin, "name", Tick(i64::MAX)).unwrap(),
        [Value::I64(42)]
    );
}

#[test]
fn rename_without_reuse_removes_old_name_at_every_canon() {
    let mut world = World::in_memory(schema()).unwrap();
    let edwin = world.create_entity("CHARACTER", None).unwrap();
    world
        .assert(edwin, "name", Value::Str("Edwin".into()), Some(Tick(0)))
        .unwrap();
    let before = world.latest_tx();
    let mut tx = world.begin();
    tx.rename_attribute("name", "title").unwrap();
    tx.commit().unwrap();
    for canon in [before, world.latest_tx()] {
        assert!(world.at_canon(canon).as_of(edwin, "name", Tick(0)).is_err());
        assert_eq!(
            world
                .at_canon(canon)
                .as_of(edwin, "title", Tick(0))
                .unwrap(),
            [Value::Str("Edwin".into())]
        );
    }
}

#[test]
fn reserved_names_and_shared_inverse_namespace_are_enforced() {
    let mut world = World::in_memory(schema()).unwrap();
    for name in ["handle", "age@old", "@hidden", ""] {
        let mut tx = world.begin();
        assert!(
            tx.declare_attribute(attr(name, ValueType::I64)).is_err(),
            "{name}"
        );
        drop(tx);
        let mut tx = world.begin();
        assert!(tx.infer_attribute(name, &Value::I64(42)).is_err(), "{name}");
        drop(tx);
        let mut tx = world.begin();
        assert!(tx.rename_attribute("name", name).is_err(), "{name}");
    }
    let mut tx = world.begin();
    tx.declare_attribute(attr("parent_of", ValueType::Ref).inverse("child_of"))
        .unwrap();
    tx.commit().unwrap();
    for name in ["located_in", "child_of"] {
        let mut tx = world.begin();
        assert!(tx.rename_attribute("name", name).is_err(), "{name}");
    }
    let mut tx = world.begin();
    assert!(
        tx.declare_attribute(attr("child_of", ValueType::I64))
            .is_err()
    );
}

#[test]
fn deprecation_refuses_assert_retract_and_every_correction_but_keeps_reads() {
    let mut world = World::in_memory(schema()).unwrap();
    let edwin = world.create_entity("CHARACTER", None).unwrap();
    let written = world
        .assert(edwin, "name", Value::Str("Edwin".into()), Some(Tick(-300)))
        .unwrap();
    let entry = written.entries[0];
    let before = world.latest_tx();
    let mut tx = world.begin();
    tx.deprecate_attribute("name").unwrap();
    let deprecated = tx.commit().unwrap();
    for canon in [before, deprecated.tx] {
        assert_eq!(
            world.at_canon(canon).as_of(edwin, "name", Tick(0)).unwrap(),
            [Value::Str("Edwin".into())]
        );
    }
    assert!(!world.at_canon(before).attribute("name").unwrap().deprecated);
    assert!(world.attribute("name").unwrap().deprecated);
    assert!(
        world
            .assert(edwin, "name", Value::Str("X".into()), Some(Tick(0)))
            .is_err()
    );
    assert!(
        world
            .retract(edwin, "name", Value::Str("Edwin".into()), Some(Tick(0)))
            .is_err()
    );
    assert!(world.correct(entry, Correction::Delete).is_err());
    assert!(
        world
            .correct(
                entry,
                Correction::Replace {
                    value: None,
                    tick: None,
                    op: None,
                    precision: None
                }
            )
            .is_err()
    );
    let mut tx = world.begin();
    tx.deprecate_attribute("name").unwrap();
    assert_eq!(tx.commit().unwrap().tx, deprecated.tx);
    let mut tx = world.begin();
    assert!(tx.declare_attribute(attr("name", ValueType::Str)).is_err());
    drop(tx);
    let mut tx = world.begin();
    tx.rename_attribute("name", "old_name").unwrap();
    tx.commit().unwrap();
    assert!(world.attribute("old_name").unwrap().deprecated);
}

#[test]
fn cardinality_change_reports_overlaps_and_old_canon_keeps_old_label() {
    let mut world = World::in_memory(schema()).unwrap();
    let edwin = world.create_entity("CHARACTER", None).unwrap();
    world
        .assert(edwin, "name", Value::Str("Edwin".into()), Some(Tick(-300)))
        .unwrap();
    world
        .assert(edwin, "name", Value::Str("King".into()), Some(Tick(-280)))
        .unwrap();
    let old = world.latest_tx();
    let mut tx = world.begin();
    tx.set_cardinality("name", Cardinality::Many).unwrap();
    let many = tx.commit().unwrap();
    assert_eq!(many.warnings.resolved.len(), 1);
    assert!(world.overlaps("name").unwrap().is_empty());
    let report = doctor(
        &world,
        &DoctorOptions {
            canon: Some(old),
            ..DoctorOptions::default()
        },
    );
    assert_eq!(
        report
            .findings
            .iter()
            .filter(|f| matches!(f, Finding::CardinalityOverlap(_)))
            .count(),
        1
    );
    let mut tx = world.begin();
    tx.set_cardinality("name", Cardinality::One).unwrap();
    let one = tx.commit().unwrap();
    assert_eq!(one.warnings.introduced.len(), 1);
    assert_eq!(
        world
            .at_canon(many.tx)
            .attribute("name")
            .unwrap()
            .cardinality,
        Cardinality::Many
    );
    assert_eq!(
        world.attribute("name").unwrap().cardinality,
        Cardinality::One
    );
}

#[test]
fn structural_redefinition_is_allowed_only_before_any_fact_was_logged() {
    let mut world = World::in_memory(schema()).unwrap();
    let edwin = world.create_entity("CHARACTER", None).unwrap();
    let mut tx = world.begin();
    tx.declare_attribute(attr("name", ValueType::I64)).unwrap();
    tx.commit().unwrap();
    assert_eq!(
        world
            .at_canon(TxId(0))
            .attribute("name")
            .unwrap()
            .value_type,
        ValueType::Str
    );
    assert_eq!(world.attribute("name").unwrap().value_type, ValueType::I64);
    let entry = world
        .assert(edwin, "name", Value::I64(42), Some(Tick(0)))
        .unwrap()
        .entries[0];
    world.correct(entry, Correction::Delete).unwrap();
    for def in [
        attr("name", ValueType::Str),
        AttrDef::new("name", ValueType::I64, Kind::Event, Cardinality::One),
        attr("name", ValueType::I64).unindexed(),
    ] {
        let mut tx = world.begin();
        let error = tx.declare_attribute(def).unwrap_err().to_string();
        assert!(error.contains("migrate"), "{error}");
    }
}

#[test]
fn staged_fact_also_prevents_structural_redefinition_and_deprecation_is_immediate() {
    let mut world = World::in_memory(schema()).unwrap();
    let edwin = world.create_entity("CHARACTER", None).unwrap();
    let mut tx = world.begin();
    tx.assert(edwin, "name", Value::Str("Edwin".into()), Some(Tick(0)))
        .unwrap();
    assert!(tx.declare_attribute(attr("name", ValueType::I64)).is_err());
    assert!(tx.commit().is_err());
    let mut tx = world.begin();
    tx.deprecate_attribute("name").unwrap();
    assert!(
        tx.assert(edwin, "name", Value::Str("Edwin".into()), Some(Tick(0)))
            .is_err()
    );
    assert!(tx.commit().is_err());
    assert!(!world.attribute("name").unwrap().deprecated);
}

#[test]
fn inferred_long_indexed_string_explains_explicit_unindexed_declaration() {
    let mut world = World::in_memory(schema()).unwrap();
    let edwin = world.create_entity("CHARACTER", None).unwrap();
    let value = Value::Str("x".repeat(257));
    let mut tx = world.begin();
    let error = match tx.infer_attribute("bio", &value) {
        Err(e) => e,
        Ok(()) => tx.assert(edwin, "bio", value, Some(Tick(0))).unwrap_err(),
    };
    assert!(
        error
            .to_string()
            .contains("[:attribute :bio :string :unindexed]"),
        "{error}"
    );
    drop(tx);
    assert!(world.attribute("bio").is_none());
}

fn migration_type(value_type: ValueType) -> MigrationOptions {
    MigrationOptions {
        value_type: Some(value_type),
        ..MigrationOptions::default()
    }
}

fn value_world(value_type: ValueType) -> (World, EntityId) {
    let mut w = World::in_memory(Schema::new().node_type("CHARACTER").attribute(AttrDef::new(
        "value",
        value_type,
        Kind::State,
        Cardinality::Many,
    )))
    .unwrap();
    let e = w.create_entity("CHARACTER", Some("edwin")).unwrap();
    (w, e)
}

#[test]
fn migration_conversion_table_accepts_exact_values() {
    for (from, value, to, expected) in [
        (
            ValueType::I64,
            Value::I64(1_i64 << 53),
            ValueType::F64,
            Value::F64((1_i64 << 53) as f64),
        ),
        (
            ValueType::I64,
            Value::I64(-(1_i64 << 53)),
            ValueType::F64,
            Value::F64(-(1_i64 << 53) as f64),
        ),
        (
            ValueType::F64,
            Value::F64(42.0),
            ValueType::I64,
            Value::I64(42),
        ),
        (
            ValueType::F64,
            Value::F64(i64::MIN as f64),
            ValueType::I64,
            Value::I64(i64::MIN),
        ),
        (
            ValueType::Bool,
            Value::Bool(false),
            ValueType::I64,
            Value::I64(0),
        ),
        (
            ValueType::Bool,
            Value::Bool(true),
            ValueType::I64,
            Value::I64(1),
        ),
        (
            ValueType::I64,
            Value::I64(0),
            ValueType::Bool,
            Value::Bool(false),
        ),
        (
            ValueType::I64,
            Value::I64(1),
            ValueType::Bool,
            Value::Bool(true),
        ),
        (
            ValueType::I64,
            Value::I64(i64::MIN),
            ValueType::Str,
            Value::Str(i64::MIN.to_string()),
        ),
        (
            ValueType::F64,
            Value::F64(1.25),
            ValueType::Str,
            Value::Str("1.25".into()),
        ),
        (
            ValueType::F64,
            Value::F64(f64::INFINITY),
            ValueType::Str,
            Value::Str("inf".into()),
        ),
        (
            ValueType::Bool,
            Value::Bool(true),
            ValueType::Str,
            Value::Str("true".into()),
        ),
        (
            ValueType::Str,
            Value::Str("42".into()),
            ValueType::I64,
            Value::I64(42),
        ),
        (
            ValueType::Str,
            Value::Str("1.25".into()),
            ValueType::F64,
            Value::F64(1.25),
        ),
        (
            ValueType::Str,
            Value::Str("inf".into()),
            ValueType::F64,
            Value::F64(f64::INFINITY),
        ),
        (
            ValueType::Str,
            Value::Str("false".into()),
            ValueType::Bool,
            Value::Bool(false),
        ),
        (
            ValueType::Str,
            Value::Str("same".into()),
            ValueType::Str,
            Value::Str("same".into()),
        ),
    ] {
        let (mut world, e) = value_world(from);
        world
            .assert(e, "value", value.clone(), Some(Tick(-300)))
            .unwrap();
        let before = world.latest_tx();
        let mut tx = world.begin();
        let report = tx
            .migrate_attribute("value", migration_type(to))
            .unwrap_or_else(|e| panic!("{from:?} {value:?} -> {to:?}: {e}"));
        assert_eq!(report.change_points, 1);
        assert_eq!(report.skipped, 0);
        tx.commit().unwrap();
        assert_eq!(world.as_of(e, "value", Tick(-300)).unwrap(), [expected]);
        assert_eq!(
            world
                .at_canon(before)
                .as_of(e, "value@old", Tick(-300))
                .unwrap(),
            [value]
        );
        assert!(
            world
                .at_canon(before)
                .as_of(e, "value", Tick(-300))
                .is_err()
        );
        assert!(world.attribute("value@old").unwrap().deprecated);
    }
}

#[test]
fn migration_conversion_table_refuses_lossy_or_invalid_values_atomically() {
    for (from, value, to) in [
        (
            ValueType::I64,
            Value::I64((1_i64 << 53) + 1),
            ValueType::F64,
        ),
        (
            ValueType::I64,
            Value::I64(-(1_i64 << 53) - 1),
            ValueType::F64,
        ),
        (ValueType::F64, Value::F64(1.5), ValueType::I64),
        (ValueType::F64, Value::F64(f64::INFINITY), ValueType::I64),
        (
            ValueType::F64,
            Value::F64(f64::NEG_INFINITY),
            ValueType::I64,
        ),
        (
            ValueType::F64,
            Value::F64(9_223_372_036_854_775_808.0),
            ValueType::I64,
        ),
        (ValueType::I64, Value::I64(2), ValueType::Bool),
        (ValueType::I64, Value::I64(-1), ValueType::Bool),
        (ValueType::Str, Value::Str(" 42".into()), ValueType::I64),
        (ValueType::Str, Value::Str("42 ".into()), ValueType::I64),
        (
            ValueType::Str,
            Value::Str("9223372036854775808".into()),
            ValueType::I64,
        ),
        (ValueType::Str, Value::Str(" 1.25".into()), ValueType::F64),
        (ValueType::Str, Value::Str("NaN".into()), ValueType::F64),
        (ValueType::Str, Value::Str("True".into()), ValueType::Bool),
        (ValueType::Str, Value::Str("1".into()), ValueType::Bool),
    ] {
        let (mut world, e) = value_world(from);
        world
            .assert(e, "value", value.clone(), Some(Tick(0)))
            .unwrap();
        let before = world.latest_tx();
        let mut tx = world.begin();
        let error = tx
            .migrate_attribute("value", migration_type(to))
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("1") && error.contains("unconvertible"),
            "{from:?} {value:?} -> {to:?}: {error}"
        );
        assert!(tx.commit().is_err());
        assert_eq!(world.latest_tx(), before);
        assert!(world.attribute("value@old").is_none());
        assert_eq!(world.as_of(e, "value", Tick(0)).unwrap(), [value]);
    }
}

#[test]
fn indexed_string_migration_counts_long_change_points_and_skip_preserves_short_ones() {
    let mut world = World::in_memory(Schema::new().node_type("CHARACTER").attribute(
        AttrDef::new("value", ValueType::Str, Kind::State, Cardinality::Many).unindexed(),
    ))
    .unwrap();
    let e = world.create_entity("CHARACTER", None).unwrap();
    world
        .assert(e, "value", Value::Str("x".repeat(257)), Some(Tick(0)))
        .unwrap();
    world
        .assert(e, "value", Value::Str("short".into()), Some(Tick(1)))
        .unwrap();
    let mut tx = world.begin();
    assert!(
        tx.migrate_attribute(
            "value",
            MigrationOptions {
                indexed: Some(true),
                ..MigrationOptions::default()
            }
        )
        .is_err()
    );
    drop(tx);
    let mut tx = world.begin();
    let report = tx
        .migrate_attribute(
            "value",
            MigrationOptions {
                indexed: Some(true),
                skip_unconvertible: true,
                ..MigrationOptions::default()
            },
        )
        .unwrap();
    assert_eq!((report.change_points, report.skipped), (1, 1));
    tx.commit().unwrap();
    assert_eq!(
        world.as_of(e, "value", Tick(1)).unwrap(),
        [Value::Str("short".into())]
    );
}

#[test]
fn migration_collisions_skip_both_old_values_but_not_other_entities() {
    let (mut world, e) = value_world(ValueType::Str);
    let other = world.create_entity("CHARACTER", Some("greg")).unwrap();
    for (who, value, tick) in [(e, "42", 0), (e, "042", 1), (e, "7", 2), (other, "42", 0)] {
        world
            .assert(who, "value", Value::Str(value.into()), Some(Tick(tick)))
            .unwrap();
    }
    let mut tx = world.begin();
    let error = tx
        .migrate_attribute("value", migration_type(ValueType::I64))
        .unwrap_err()
        .to_string();
    assert!(
        error.contains('2') && error.contains("unconvertible"),
        "{error}"
    );
    drop(tx);
    let mut tx = world.begin();
    let report = tx
        .migrate_attribute(
            "value",
            MigrationOptions {
                skip_unconvertible: true,
                ..migration_type(ValueType::I64)
            },
        )
        .unwrap();
    assert_eq!((report.change_points, report.skipped), (2, 2));
    tx.commit().unwrap();
    assert_eq!(world.as_of(e, "value", Tick(5)).unwrap(), [Value::I64(7)]);
    assert_eq!(
        world.as_of(other, "value", Tick(5)).unwrap(),
        [Value::I64(42)]
    );
}

#[test]
fn migration_keeps_redundant_looking_history_ticks_ops_and_precision() {
    let (mut world, e) = value_world(ValueType::I64);
    // Earlier assertion is written second; both change points are author input.
    world
        .assert_with_precision(
            e,
            "value",
            Value::I64(42),
            Some(Tick(-200)),
            Precision::Year,
        )
        .unwrap();
    world
        .assert_with_precision(
            e,
            "value",
            Value::I64(42),
            Some(Tick(-300)),
            Precision::Century,
        )
        .unwrap();
    world
        .retract_with_precision(e, "value", Value::I64(42), Some(Tick(0)), Precision::Day)
        .unwrap();
    world
        .assert_with_precision(
            e,
            "value",
            Value::I64(42),
            Some(Tick(i64::MAX)),
            Precision::Exact,
        )
        .unwrap();
    let old = world.history(e, "value").unwrap();
    assert_eq!(old.len(), 4);
    let mut tx = world.begin();
    let report = tx
        .migrate_attribute("value", migration_type(ValueType::Str))
        .unwrap();
    assert_eq!(report.change_points, 4);
    tx.commit().unwrap();
    let migrated = world.history(e, "value").unwrap();
    assert_eq!(migrated.len(), old.len());
    for (old, new) in old.iter().zip(&migrated) {
        assert_eq!(
            (new.tick, new.op, new.precision),
            (old.tick, old.op, old.precision)
        );
        assert_eq!(new.value, Value::Str("42".into()));
    }
    assert_eq!(world.history(e, "value@old").unwrap(), old);
}

#[test]
fn migration_copies_latest_corrections_and_preserves_old_canon_trail() {
    let (mut world, e) = value_world(ValueType::I64);
    let entry = world
        .assert(e, "value", Value::I64(42), Some(Tick(-300)))
        .unwrap()
        .entries[0];
    let original = world.latest_tx();
    world
        .correct(
            entry,
            Correction::Replace {
                value: Some(Value::I64(43)),
                tick: Some(Tick(-200)),
                op: None,
                precision: Some(Precision::Year),
            },
        )
        .unwrap();
    let mut tx = world.begin();
    tx.migrate_attribute("value", migration_type(ValueType::Str))
        .unwrap();
    tx.commit().unwrap();
    let history = world.history(e, "value").unwrap();
    assert_eq!(history.len(), 1);
    assert_eq!(
        (history[0].tick, history[0].precision),
        (Tick(-200), Precision::Year)
    );
    assert_eq!(history[0].value, Value::Str("43".into()));
    assert_eq!(
        world
            .at_canon(original)
            .as_of(e, "value@old", Tick(-250))
            .unwrap(),
        [Value::I64(42)]
    );
    assert!(world.as_of(e, "value@old", Tick(-250)).unwrap().is_empty());
    let mut tx = world.begin();
    tx.migrate_attribute("value", migration_type(ValueType::I64))
        .unwrap();
    tx.commit().unwrap();
    assert!(world.attribute("value@old").unwrap().deprecated);
    assert!(world.attribute("value@old2").unwrap().deprecated);
    assert_eq!(world.as_of(e, "value", Tick(0)).unwrap(), [Value::I64(43)]);
}

#[test]
fn event_to_state_migrates_but_any_logged_retract_blocks_state_to_event() {
    let mut world = World::in_memory(Schema::new().node_type("CHARACTER").attribute(AttrDef::new(
        "value",
        ValueType::I64,
        Kind::Event,
        Cardinality::Many,
    )))
    .unwrap();
    let e = world.create_entity("CHARACTER", None).unwrap();
    world
        .assert(e, "value", Value::I64(42), Some(Tick(-300)))
        .unwrap();
    let mut tx = world.begin();
    tx.migrate_attribute(
        "value",
        MigrationOptions {
            kind: Some(Kind::State),
            ..MigrationOptions::default()
        },
    )
    .unwrap();
    tx.commit().unwrap();
    let retract = world
        .retract(e, "value", Value::I64(42), Some(Tick(0)))
        .unwrap()
        .entries[0];
    world.correct(retract, Correction::Delete).unwrap();
    let mut tx = world.begin();
    let error = tx
        .migrate_attribute(
            "value",
            MigrationOptions {
                kind: Some(Kind::Event),
                ..MigrationOptions::default()
            },
        )
        .unwrap_err()
        .to_string();
    assert!(error.contains("retract"), "{error}");
}

fn edge_world(direction: RefDirection) -> (World, EntityId, EntityId) {
    let mut relation = AttrDef::new("link", ValueType::Ref, Kind::State, Cardinality::Many);
    relation.direction = Some(direction);
    let mut world = World::in_memory(
        Schema::new()
            .node_type("CHARACTER")
            .attribute(relation)
            .attribute(attr("disguised", ValueType::Bool)),
    )
    .unwrap();
    let a = world.create_entity("CHARACTER", Some("edwin")).unwrap();
    let b = world.create_entity("CHARACTER", Some("mara")).unwrap();
    (world, a, b)
}

#[test]
fn directed_to_mutual_migration_copies_edge_property_history_to_new_edge() {
    let (mut world, a, b) = edge_world(RefDirection::Directed);
    world
        .assert(a, "link", Value::Ref(b), Some(Tick(-300)))
        .unwrap();
    let edge = world.edge_id(a, "link", b).unwrap();
    world
        .assert(
            edge.into(),
            "disguised",
            Value::Bool(true),
            Some(Tick(-300)),
        )
        .unwrap();
    world
        .retract(
            edge.into(),
            "disguised",
            Value::Bool(true),
            Some(Tick(-280)),
        )
        .unwrap();
    world
        .assert(
            edge.into(),
            "disguised",
            Value::Bool(false),
            Some(Tick(-280)),
        )
        .unwrap();
    let old_properties = world.history(edge.into(), "disguised").unwrap();
    let mut tx = world.begin();
    let report = tx
        .migrate_attribute(
            "link",
            MigrationOptions {
                direction: Some(RefDirection::Bidirectional),
                ..MigrationOptions::default()
            },
        )
        .unwrap();
    assert_eq!(
        (report.change_points, report.edges, report.edge_properties),
        (1, 1, 3)
    );
    tx.commit().unwrap();
    let new_edge = world.edge_id(a, "link", b).unwrap();
    assert_ne!(new_edge, edge);
    assert_eq!(world.edge_id(b, "link", a), Some(new_edge));
    assert_eq!(
        world.history(new_edge.into(), "disguised").unwrap(),
        old_properties
    );
    assert_eq!(world.as_of(b, "link", Tick(-290)).unwrap(), [Value::Ref(a)]);
}

#[test]
fn mutual_to_directed_migration_keeps_both_orientations_and_properties() {
    let (mut world, a, b) = edge_world(RefDirection::Bidirectional);
    world
        .assert(a, "link", Value::Ref(b), Some(Tick(-300)))
        .unwrap();
    world
        .retract(a, "link", Value::Ref(b), Some(Tick(-200)))
        .unwrap();
    let old_edge = world.edge_id(a, "link", b).unwrap();
    world
        .assert(
            old_edge.into(),
            "disguised",
            Value::Bool(true),
            Some(Tick(-300)),
        )
        .unwrap();
    let mut tx = world.begin();
    let report = tx
        .migrate_attribute(
            "link",
            MigrationOptions {
                direction: Some(RefDirection::Directed),
                ..MigrationOptions::default()
            },
        )
        .unwrap();
    assert_eq!(
        (report.change_points, report.edges, report.edge_properties),
        (4, 2, 2)
    );
    tx.commit().unwrap();
    let forward = world.edge_id(a, "link", b).unwrap();
    let reverse = world.edge_id(b, "link", a).unwrap();
    assert_ne!(forward, reverse);
    for (source, target, edge) in [(a, b, forward), (b, a, reverse)] {
        assert_ne!(edge, old_edge);
        assert_eq!(
            world.as_of(source, "link", Tick(-208)).unwrap(),
            [Value::Ref(target)]
        );
        assert!(world.as_of(source, "link", Tick(-200)).unwrap().is_empty());
        assert_eq!(
            world.as_of(edge.into(), "disguised", Tick(-208)).unwrap(),
            [Value::Bool(true)]
        );
    }
}

#[test]
fn two_directed_histories_refuse_mutual_even_when_one_is_retracted() {
    let (mut world, a, b) = edge_world(RefDirection::Directed);
    world
        .assert(a, "link", Value::Ref(b), Some(Tick(-300)))
        .unwrap();
    world
        .assert(b, "link", Value::Ref(a), Some(Tick(-280)))
        .unwrap();
    world
        .retract(b, "link", Value::Ref(a), Some(Tick(-200)))
        .unwrap();
    let mut tx = world.begin();
    let error = tx
        .migrate_attribute(
            "link",
            MigrationOptions {
                direction: Some(RefDirection::Bidirectional),
                ..MigrationOptions::default()
            },
        )
        .unwrap_err()
        .to_string();
    assert!(error.contains("both") || error.contains("pair"), "{error}");
}

#[test]
fn ref_and_nonref_migration_is_refused_even_with_skip() {
    let (mut world, a, b) = edge_world(RefDirection::Directed);
    world
        .assert(a, "link", Value::Ref(b), Some(Tick(0)))
        .unwrap();
    for skip in [false, true] {
        let mut tx = world.begin();
        assert!(
            tx.migrate_attribute(
                "link",
                MigrationOptions {
                    skip_unconvertible: skip,
                    ..migration_type(ValueType::Str)
                }
            )
            .is_err()
        );
        drop(tx);
        let mut tx = world.begin();
        assert!(
            tx.migrate_attribute(
                "disguised",
                MigrationOptions {
                    skip_unconvertible: skip,
                    ..migration_type(ValueType::Ref)
                }
            )
            .is_err()
        );
    }
}

#[test]
fn staged_write_then_migrate_replays_the_same_history() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("world.kala");
    let mut world = World::create(&path, schema()).unwrap();
    let e = world.create_entity("CHARACTER", Some("edwin")).unwrap();
    let mut tx = world.begin();
    tx.infer_attribute("age", &Value::I64(42)).unwrap();
    tx.assert(e, "age", Value::I64(42), Some(Tick(-300)))
        .unwrap();
    tx.migrate_attribute("age", migration_type(ValueType::Str))
        .unwrap();
    tx.assert(e, "age", Value::Str("43".into()), Some(Tick(0)))
        .unwrap();
    tx.commit().unwrap();
    assert_eq!(
        world.as_of(e, "age", Tick(-300)).unwrap(),
        [Value::Str("42".into())]
    );
    let reopened = World::open_read_only(&path)
        .expect("a staged source fact must be visible while replay validates its migration");
    assert_eq!(world.index_dump(), reopened.index_dump());
    assert_eq!(
        world.history(e, "age").unwrap(),
        reopened.history(e, "age").unwrap()
    );
    assert_eq!(
        world.history(e, "age@old").unwrap(),
        reopened.history(e, "age@old").unwrap()
    );
    assert!(
        doctor(&reopened, &DoctorOptions::default())
            .findings
            .iter()
            .all(|f| matches!(f, Finding::CardinalityOverlap(_)))
    );
}

#[test]
fn a_second_migration_in_one_transaction_replays_prior_copy_points() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("world.kala");
    let mut world = World::create(&path, schema().attribute(attr("age", ValueType::I64))).unwrap();
    let e = world.create_entity("CHARACTER", None).unwrap();
    world
        .assert(e, "age", Value::I64(42), Some(Tick(0)))
        .unwrap();
    let mut tx = world.begin();
    tx.migrate_attribute("age", migration_type(ValueType::Str))
        .unwrap();
    tx.migrate_attribute("age", migration_type(ValueType::I64))
        .unwrap();
    tx.commit().unwrap();
    let reopened = World::open_read_only(&path).unwrap();
    assert_eq!(world.index_dump(), reopened.index_dump());
    assert_eq!(reopened.as_of(e, "age", Tick(0)).unwrap(), [Value::I64(42)]);
    assert!(reopened.attribute("age@old").unwrap().deprecated);
    assert!(reopened.attribute("age@old2").unwrap().deprecated);
}

#[test]
fn migration_of_retract_only_ref_history_interns_replayable_new_edge() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("world.kala");
    let mut world = World::create(&path, schema()).unwrap();
    let a = world.create_entity("CHARACTER", Some("edwin")).unwrap();
    let b = world.create_entity("PLACE", Some("varrow")).unwrap();
    let entry = world
        .assert(a, "located_in", Value::Ref(b), Some(Tick(-300)))
        .unwrap()
        .entries[0];
    world
        .retract(a, "located_in", Value::Ref(b), Some(Tick(-200)))
        .unwrap();
    world.correct(entry, Correction::Delete).unwrap();
    let before = world.history(a, "located_in").unwrap();
    assert_eq!(before.len(), 1);
    assert_eq!(before[0].op, Op::Retract);
    let mut tx = world.begin();
    let report = tx
        .migrate_attribute(
            "located_in",
            MigrationOptions {
                cardinality: Some(Cardinality::Many),
                ..MigrationOptions::default()
            },
        )
        .unwrap();
    assert_eq!((report.change_points, report.edges), (1, 1));
    tx.commit().unwrap();
    assert_eq!(world.history(a, "located_in").unwrap(), before);
    let reopened = World::open_read_only(&path).unwrap();
    assert_eq!(world.index_dump(), reopened.index_dump());
}

#[test]
fn migration_transfers_inverse_to_new_attribute_and_old_canons_keep_latest_namespace() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("world.kala");
    let mut world = World::create(
        &path,
        Schema::new().node_type("CHARACTER").attribute(
            attr("parent_of", ValueType::Ref)
                .inverse("child_of")
                .reverse_one(),
        ),
    )
    .unwrap();
    let a = world.create_entity("CHARACTER", Some("edwin")).unwrap();
    let b = world.create_entity("CHARACTER", Some("mara")).unwrap();
    world
        .assert(a, "parent_of", Value::Ref(b), Some(Tick(-300)))
        .unwrap();
    let before = world.latest_tx();
    let mut tx = world.begin();
    tx.migrate_attribute(
        "parent_of",
        MigrationOptions {
            cardinality: Some(Cardinality::Many),
            ..MigrationOptions::default()
        },
    )
    .unwrap();
    tx.commit().unwrap();
    assert_eq!(
        world.attribute("parent_of").unwrap().inverse.as_deref(),
        Some("child_of")
    );
    assert_eq!(
        world.attribute("parent_of").unwrap().reverse_cardinality,
        Some(Cardinality::One)
    );
    assert!(world.attribute("parent_of@old").unwrap().inverse.is_none());
    assert!(
        world
            .at_canon(before)
            .neighbors(b, "child_of", Direction::Out, Tick(0))
            .is_err()
    );
    let reopened = World::open_read_only(&path).unwrap();
    assert_eq!(world.index_dump(), reopened.index_dump());
    assert_eq!(
        reopened.as_of(a, "parent_of", Tick(0)).unwrap(),
        [Value::Ref(b)]
    );
}

#[test]
fn failed_schema_append_restores_definitions_names_settings_and_ids() {
    for fault in [LogFault::FailNextSync, LogFault::FailNextWrite] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("world.kala");
        let mut world = World::create(&path, schema()).unwrap();
        let before = std::fs::read(&path).unwrap();
        world.inject_log_fault(fault);
        let mut tx = world.begin();
        tx.rename_attribute("name", "title").unwrap();
        tx.infer_attribute("age", &Value::I64(42)).unwrap();
        tx.infer_node_type("FACTION").unwrap();
        tx.create_entity("FACTION", Some("red_hand")).unwrap();
        tx.world_setting("schema", "strict").unwrap();
        assert!(matches!(tx.commit(), Err(Error::Io { .. })));
        assert_eq!(std::fs::read(&path).unwrap(), before);
        assert!(world.attribute("name").is_some());
        assert!(world.attribute("title").is_none());
        assert!(world.attribute("age").is_none());
        assert_eq!(world.world_setting("schema"), Some("infer"));
        assert!(world.entity_by_handle("red_hand").is_none());
        assert!(!world.node_types().any(|n| n == "FACTION"));
        assert_eq!(world.latest_tx(), TxId(0));
        let reopened = World::open_read_only(&path).unwrap();
        assert_eq!(world.index_dump(), reopened.index_dump());
    }
}

#[test]
fn migrating_relationship_used_as_its_own_edge_property_copies_under_new_name() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("world.kala");
    let mut world = World::create(
        &path,
        Schema::new().node_type("CHARACTER").attribute(AttrDef::new(
            "link",
            ValueType::Ref,
            Kind::State,
            Cardinality::Many,
        )),
    )
    .unwrap();
    let a = world.create_entity("CHARACTER", Some("edwin")).unwrap();
    let b = world.create_entity("CHARACTER", Some("mara")).unwrap();
    let c = world.create_entity("CHARACTER", Some("greg")).unwrap();
    world
        .assert(a, "link", Value::Ref(b), Some(Tick(-300)))
        .unwrap();
    let old_edge = world.edge_id(a, "link", b).unwrap();
    world
        .assert(old_edge.into(), "link", Value::Ref(c), Some(Tick(-280)))
        .unwrap();
    let mut tx = world.begin();
    let migrated = tx.migrate_attribute(
        "link",
        MigrationOptions {
            cardinality: Some(Cardinality::One),
            ..MigrationOptions::default()
        },
    );
    assert!(
        migrated.is_ok(),
        "the migrated attribute can itself be an edge property: {migrated:?}"
    );
    assert_eq!(migrated.unwrap().edge_properties, 1);
    tx.commit().unwrap();
    let new_edge = world.edge_id(a, "link", b).unwrap();
    assert_ne!(old_edge, new_edge);
    assert_eq!(
        world.as_of(new_edge.into(), "link", Tick(-270)).unwrap(),
        [Value::Ref(c)]
    );
    assert_eq!(
        world
            .as_of(old_edge.into(), "link@old", Tick(-270))
            .unwrap(),
        [Value::Ref(c)]
    );
    let reopened = World::open_read_only(&path).unwrap();
    assert_eq!(world.index_dump(), reopened.index_dump());
}

#[test]
fn world_settings_and_node_origins_are_read_per_canon_and_survive_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("world.kala");
    let mut world = World::create(&path, schema()).unwrap();
    let mut tx = world.begin();
    tx.infer_node_type("FACTION").unwrap();
    tx.world_setting("schema", "strict").unwrap();
    tx.world_setting("concurrency", "weak").unwrap();
    let first = tx.commit().unwrap().tx;
    let mut tx = world.begin();
    tx.declare_node_type("FACTION").unwrap();
    tx.world_setting("schema", "infer").unwrap();
    tx.world_setting("concurrency", "optimistic").unwrap();
    let second = tx.commit().unwrap().tx;
    let reopened = World::open_read_only(&path).unwrap();
    for world in [&world, &reopened] {
        let genesis = world.at_canon(TxId(0));
        assert_eq!(genesis.settings().get("schema"), Some(&"infer"));
        assert!(!genesis.settings().contains_key("concurrency"));
        assert!(
            !genesis
                .node_type_definitions()
                .any(|(name, _, _)| name == "FACTION")
        );
        let inferred = world.at_canon(first);
        assert_eq!(inferred.settings().get("schema"), Some(&"strict"));
        assert_eq!(inferred.settings().get("concurrency"), Some(&"weak"));
        assert!(
            inferred
                .node_type_definitions()
                .any(|(name, origin, defined)| name == "FACTION" && origin && defined == first)
        );
        let declared = world.at_canon(second);
        assert_eq!(declared.settings().get("schema"), Some(&"infer"));
        assert_eq!(declared.settings().get("concurrency"), Some(&"optimistic"));
        assert!(
            declared
                .node_type_definitions()
                .any(|(name, origin, defined)| name == "FACTION" && !origin && defined == first)
        );
    }
    let mut tx = world.begin();
    tx.world_setting("schema", "infer").unwrap();
    tx.world_setting("concurrency", "optimistic").unwrap();
    let same = tx.commit().unwrap();
    assert_eq!(same.tx, second);
    assert!(same.schema.is_empty());
}

#[test]
fn final_audit_reads_use_old_ref_definition_after_factless_type_change() {
    let mut world = World::in_memory(schema()).unwrap();
    let a = world.create_entity("CHARACTER", Some("edwin")).unwrap();
    let b = world.create_entity("PLACE", Some("varrow")).unwrap();
    let before = world.latest_tx();
    let mut tx = world.begin();
    tx.declare_attribute(attr("located_in", ValueType::I64))
        .unwrap();
    tx.commit().unwrap();
    let old = world.at_canon(before);
    assert_eq!(
        old.attribute("located_in").unwrap().value_type,
        ValueType::Ref
    );
    for direction in [Direction::Out, Direction::In, Direction::Both] {
        assert_eq!(
            old.neighbors(a, "located_in", direction, Tick(0)).unwrap(),
            []
        );
    }
    assert_eq!(
        old.lookup("located_in", &Value::Ref(b), Tick(0)).unwrap(),
        []
    );
    assert_eq!(old.as_of(a, "located_in", Tick(0)).unwrap(), []);
    assert_eq!(old.history(a, "located_in").unwrap(), []);
    assert_eq!(old.history_scan("located_in").unwrap(), []);
    let meter = WorkMeter::new();
    assert_eq!(
        old.metered(&meter)
            .neighbors(a, "located_in", Direction::Both, Tick(0))
            .unwrap(),
        []
    );
    assert!(matches!(
        world.neighbors(a, "located_in", Direction::Out, Tick(0)),
        Err(Error::NotARef(_))
    ));
}

#[test]
fn final_audit_rename_preserves_overlap_warnings_without_false_introduced_or_resolved() {
    let mut world = World::in_memory(schema()).unwrap();
    let e = world.create_entity("CHARACTER", None).unwrap();
    world
        .assert(e, "name", Value::Str("Edwin".into()), Some(Tick(-300)))
        .unwrap();
    world
        .assert(e, "name", Value::Str("King".into()), Some(Tick(-280)))
        .unwrap();
    let before = world.latest_tx();
    assert_eq!(world.overlaps("name").unwrap().len(), 1);
    let mut tx = world.begin();
    tx.rename_attribute("name", "title").unwrap();
    let committed = tx.commit().unwrap();
    assert!(
        committed.warnings.introduced.is_empty() && committed.warnings.resolved.is_empty(),
        "renaming labels cannot create or resolve an overlap: {:?}",
        committed.warnings
    );
    assert_eq!(
        world.overlaps("title").unwrap(),
        world.at_canon(before).overlaps("title").unwrap()
    );
}

#[test]
fn final_audit_doctor_since_includes_overlaps_introduced_by_schema_only_change() {
    let mut world = World::in_memory(Schema::new().node_type("CHARACTER").attribute(AttrDef::new(
        "age",
        ValueType::I64,
        Kind::State,
        Cardinality::Many,
    )))
    .unwrap();
    let e = world.create_entity("CHARACTER", None).unwrap();
    world
        .assert(e, "age", Value::I64(42), Some(Tick(-300)))
        .unwrap();
    world
        .assert(e, "age", Value::I64(50), Some(Tick(-280)))
        .unwrap();
    let before = world.latest_tx();
    let mut tx = world.begin();
    tx.set_cardinality("age", Cardinality::One).unwrap();
    let changed = tx.commit().unwrap();
    assert_eq!(changed.warnings.introduced.len(), 1);
    let report = doctor(
        &world,
        &DoctorOptions {
            since: Some(before),
            ..DoctorOptions::default()
        },
    );
    let overlaps = report
        .findings
        .iter()
        .filter_map(|finding| match finding {
            Finding::CardinalityOverlap(overlap) => Some(overlap.clone()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        overlaps, changed.warnings.introduced,
        "the schema edit happened after --since even though its facts did not"
    );
    let old_report = doctor(
        &world,
        &DoctorOptions {
            canon: Some(before),
            since: Some(before),
        },
    );
    assert!(old_report.findings.is_empty(), "{old_report:?}");
}

#[test]
fn final_audit_inverse_name_is_not_defined_before_its_schema_revision() {
    let mut world = World::in_memory(
        Schema::new()
            .node_type("CHARACTER")
            .attribute(attr("parent_of", ValueType::Ref)),
    )
    .unwrap();
    let a = world.create_entity("CHARACTER", Some("edwin")).unwrap();
    let b = world.create_entity("CHARACTER", Some("mara")).unwrap();
    world
        .assert(a, "parent_of", Value::Ref(b), Some(Tick(-300)))
        .unwrap();
    let before = world.latest_tx();
    let mut tx = world.begin();
    tx.declare_attribute(attr("parent_of", ValueType::Ref).inverse("child_of"))
        .unwrap();
    tx.commit().unwrap();
    assert_eq!(
        world
            .neighbors(b, "child_of", Direction::Out, Tick(0))
            .unwrap()
            .len(),
        1
    );
    let result = world
        .at_canon(before)
        .neighbors(b, "child_of", Direction::Out, Tick(0));
    assert!(
        result.is_err(),
        "the inverse did not exist at canon tx#{}: {result:?}",
        before.0
    );
}

#[test]
fn final_audit_inverse_scans_follow_latest_binding_and_definition_at_canon() {
    let mut world = World::in_memory(
        Schema::new()
            .node_type("CHARACTER")
            .attribute(attr("parent_of", ValueType::Ref)),
    )
    .unwrap();
    let a = world.create_entity("CHARACTER", Some("edwin")).unwrap();
    let b = world.create_entity("CHARACTER", Some("mara")).unwrap();
    world
        .assert(a, "parent_of", Value::Ref(b), Some(Tick(0)))
        .unwrap();
    let before_inverse = world.latest_tx();
    let mut tx = world.begin();
    tx.declare_attribute(attr("parent_of", ValueType::Ref).inverse("child_of"))
        .unwrap();
    let before_migration = tx.commit().unwrap().tx;
    assert_eq!(
        world.attr_scan("child_of", Tick(0)).unwrap(),
        [(b, Value::Ref(a))]
    );
    assert_eq!(world.history_scan("child_of").unwrap().len(), 1);
    let old = world.at_canon(before_inverse);
    assert!(
        old.attr_scan("child_of", Tick(0)).is_err(),
        "inverse scan cannot predate its definition"
    );
    assert!(
        old.history_scan("child_of").is_err(),
        "inverse history cannot predate its definition"
    );
    let mut tx = world.begin();
    tx.migrate_attribute(
        "parent_of",
        MigrationOptions {
            cardinality: Some(Cardinality::Many),
            ..MigrationOptions::default()
        },
    )
    .unwrap();
    tx.commit().unwrap();
    assert_eq!(
        world.attr_scan("child_of", Tick(0)).unwrap(),
        [(b, Value::Ref(a))]
    );
    assert_eq!(world.history_scan("child_of").unwrap().len(), 1);
    let old = world.at_canon(before_migration);
    assert!(
        old.attr_scan("child_of", Tick(0)).is_err(),
        "the latest inverse binding belongs to the replacement attribute"
    );
    assert!(old.history_scan("child_of").is_err());
    assert!(
        old.neighbors(b, "child_of", Direction::Out, Tick(0))
            .is_err()
    );
    assert_eq!(
        old.as_of(a, "parent_of@old", Tick(0)).unwrap(),
        [Value::Ref(b)]
    );
}

#[test]
fn reverse_cardinality_redefinition_warns_and_preserves_the_older_label() {
    let definition = AttrDef::new("rules", ValueType::Ref, Kind::State, Cardinality::Many);
    let mut world = World::in_memory(
        Schema::new()
            .node_type("CHARACTER")
            .attribute(definition.clone()),
    )
    .unwrap();
    let a = world.create_entity("CHARACTER", None).unwrap();
    let b = world.create_entity("CHARACTER", None).unwrap();
    let city = world.create_entity("CHARACTER", None).unwrap();
    world
        .assert(a, "rules", Value::Ref(city), Some(Tick(-300)))
        .unwrap();
    world
        .assert(b, "rules", Value::Ref(city), Some(Tick(-280)))
        .unwrap();
    let before = world.latest_tx();
    let mut tx = world.begin();
    tx.declare_attribute(definition.clone().reverse_one())
        .unwrap();
    let one = tx.commit().unwrap();
    assert_eq!(one.warnings.introduced.len(), 1);
    let overlap = &one.warnings.introduced[0];
    assert_eq!((overlap.entity, overlap.side), (city, Side::Reverse));
    assert_eq!(overlap.during.from, Tick(-280));
    assert_eq!(
        world
            .at_canon(before)
            .attribute("rules")
            .unwrap()
            .reverse_cardinality,
        None
    );
    assert!(world.at_canon(before).overlaps("rules").unwrap().is_empty());
    let mut many = definition;
    many.reverse_cardinality = Some(Cardinality::Many);
    let mut tx = world.begin();
    tx.declare_attribute(many).unwrap();
    let many = tx.commit().unwrap();
    assert_eq!(many.warnings.resolved, one.warnings.introduced);
    assert!(world.overlaps("rules").unwrap().is_empty());
}

#[test]
fn staged_correction_then_migration_copies_corrected_history_and_replays() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("world.kala");
    let mut world = World::create(&path, schema().attribute(attr("age", ValueType::I64))).unwrap();
    let e = world.create_entity("CHARACTER", None).unwrap();
    let base = world
        .assert(e, "age", Value::I64(42), Some(Tick(-300)))
        .unwrap()
        .entries[0];
    let original = world.latest_tx();
    let mut tx = world.begin();
    tx.correct(
        base,
        Correction::Replace {
            value: Some(Value::I64(43)),
            tick: Some(Tick(-200)),
            precision: Some(Precision::Year),
            op: None,
        },
    )
    .unwrap();
    let copied = tx
        .migrate_attribute("age", migration_type(ValueType::Str))
        .unwrap();
    assert_eq!(copied.change_points, 1);
    tx.commit().unwrap();
    let reopened = World::open_read_only(&path).unwrap();
    assert_eq!(world.index_dump(), reopened.index_dump());
    assert_eq!(
        reopened.history(e, "age").unwrap(),
        [Change {
            tick: Tick(-200),
            precision: Precision::Year,
            op: Op::Assert,
            value: Value::Str("43".into())
        }]
    );
    assert_eq!(
        reopened
            .at_canon(original)
            .as_of(e, "age@old", Tick(-250))
            .unwrap(),
        [Value::I64(42)]
    );
    assert!(
        doctor(&reopened, &DoctorOptions::default())
            .findings
            .is_empty()
    );
}

#[test]
fn mutual_self_edge_migration_creates_one_directed_copy_with_properties() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("world.kala");
    let mut world = World::create(
        &path,
        Schema::new()
            .node_type("CHARACTER")
            .attribute(
                AttrDef::new("ally_of", ValueType::Ref, Kind::State, Cardinality::Many)
                    .bidirectional(),
            )
            .attribute(attr("disguised", ValueType::Bool)),
    )
    .unwrap();
    let e = world.create_entity("CHARACTER", None).unwrap();
    world
        .assert(e, "ally_of", Value::Ref(e), Some(Tick(-300)))
        .unwrap();
    let old = world.edge_id(e, "ally_of", e).unwrap();
    world
        .assert(old.into(), "disguised", Value::Bool(true), Some(Tick(-300)))
        .unwrap();
    let mut tx = world.begin();
    let copied = tx
        .migrate_attribute(
            "ally_of",
            MigrationOptions {
                direction: Some(RefDirection::Directed),
                ..MigrationOptions::default()
            },
        )
        .unwrap();
    assert_eq!(
        (copied.change_points, copied.edges, copied.edge_properties),
        (1, 1, 1)
    );
    tx.commit().unwrap();
    let new = world.edge_id(e, "ally_of", e).unwrap();
    assert_ne!(new, old);
    assert_eq!(world.as_of(e, "ally_of", Tick(0)).unwrap(), [Value::Ref(e)]);
    assert_eq!(
        world.as_of(new.into(), "disguised", Tick(0)).unwrap(),
        [Value::Bool(true)]
    );
    let reopened = World::open_read_only(&path).unwrap();
    assert_eq!(world.index_dump(), reopened.index_dump());
    assert!(
        doctor(&reopened, &DoctorOptions::default())
            .findings
            .is_empty()
    );
}

#[test]
fn deprecated_attributes_can_be_relabelled_but_never_written_or_redeclared() {
    let mut world = World::in_memory(Schema::new().node_type("CHARACTER").attribute(AttrDef::new(
        "age",
        ValueType::I64,
        Kind::State,
        Cardinality::Many,
    )))
    .unwrap();
    let e = world.create_entity("CHARACTER", None).unwrap();
    let base = world
        .assert(e, "age", Value::I64(42), Some(Tick(-300)))
        .unwrap()
        .entries[0];
    world
        .assert(e, "age", Value::I64(50), Some(Tick(-280)))
        .unwrap();
    let mut tx = world.begin();
    tx.deprecate_attribute("age").unwrap();
    let deprecated = tx.commit().unwrap().tx;
    let mut tx = world.begin();
    let result = tx.set_cardinality("age", Cardinality::One);
    assert!(
        result.is_ok(),
        "cardinality is a label that remains editable on deprecated history: {result:?}"
    );
    let changed = tx.commit().unwrap();
    assert_eq!(changed.warnings.introduced.len(), 1);
    assert!(world.attribute("age").unwrap().deprecated);
    assert_eq!(
        world.as_of(e, "age", Tick(0)).unwrap(),
        [Value::I64(42), Value::I64(50)]
    );
    assert_eq!(
        world
            .at_canon(deprecated)
            .attribute("age")
            .unwrap()
            .cardinality,
        Cardinality::Many
    );
    assert!(
        world
            .assert(e, "age", Value::I64(60), Some(Tick(0)))
            .is_err()
    );
    assert!(
        world
            .retract(e, "age", Value::I64(42), Some(Tick(0)))
            .is_err()
    );
    assert!(world.correct(base, Correction::Delete).is_err());
    let mut tx = world.begin();
    assert!(tx.declare_attribute(attr("age", ValueType::I64)).is_err());
}

fn migrate_with_mutual_edge_property(staged_property: bool) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("world.kala");
    let mut world = World::create(
        &path,
        Schema::new()
            .node_type("CHARACTER")
            .attribute(AttrDef::new(
                "link",
                ValueType::Ref,
                Kind::State,
                Cardinality::Many,
            ))
            .attribute(
                AttrDef::new("related_to", ValueType::Ref, Kind::State, Cardinality::Many)
                    .bidirectional(),
            ),
    )
    .unwrap();
    let a = world.create_entity("CHARACTER", Some("edwin")).unwrap();
    let b = world.create_entity("CHARACTER", Some("mara")).unwrap();
    let c = world.create_entity("CHARACTER", Some("greg")).unwrap();
    world
        .assert(a, "link", Value::Ref(b), Some(Tick(-300)))
        .unwrap();
    let old_edge = world.edge_id(a, "link", b).unwrap();
    let expected = vec![
        Change {
            tick: Tick(-250),
            op: Op::Assert,
            value: Value::Ref(c),
            precision: Precision::Year,
        },
        Change {
            tick: Tick(-200),
            op: Op::Retract,
            value: Value::Ref(c),
            precision: Precision::Day,
        },
        Change {
            tick: Tick(-150),
            op: Op::Assert,
            value: Value::Ref(c),
            precision: Precision::Exact,
        },
    ];
    let mut property = world.begin();
    property
        .assert_with_precision(
            old_edge.into(),
            "related_to",
            Value::Ref(c),
            Some(Tick(-250)),
            Precision::Year,
        )
        .unwrap();
    property
        .retract_with_precision(
            old_edge.into(),
            "related_to",
            Value::Ref(c),
            Some(Tick(-200)),
            Precision::Day,
        )
        .unwrap();
    property
        .assert(
            old_edge.into(),
            "related_to",
            Value::Ref(c),
            Some(Tick(-150)),
        )
        .unwrap();
    let report = if staged_property {
        let copied = property
            .migrate_attribute(
                "link",
                MigrationOptions {
                    cardinality: Some(Cardinality::One),
                    ..MigrationOptions::default()
                },
            )
            .unwrap();
        property.commit().unwrap();
        copied
    } else {
        property.commit().unwrap();
        assert_eq!(
            world.history(old_edge.into(), "related_to").unwrap(),
            expected
        );
        let mut migration = world.begin();
        let copied = migration
            .migrate_attribute(
                "link",
                MigrationOptions {
                    cardinality: Some(Cardinality::One),
                    ..MigrationOptions::default()
                },
            )
            .unwrap();
        migration.commit().unwrap();
        copied
    };
    assert_eq!(
        report.edge_properties, 3,
        "a mutual property stored with the old edge as target must be copied"
    );
    let new_edge = world.edge_id(a, "link", b).unwrap();
    assert_ne!(new_edge, old_edge);
    assert_eq!(
        world.history(new_edge.into(), "related_to").unwrap(),
        expected
    );
    assert_eq!(
        world.history(old_edge.into(), "related_to").unwrap(),
        expected
    );
    assert_eq!(
        world
            .as_of(new_edge.into(), "related_to", Tick(-225))
            .unwrap(),
        [Value::Ref(c)]
    );
    assert!(
        world
            .as_of(new_edge.into(), "related_to", Tick(-175))
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        world.as_of(new_edge.into(), "related_to", Tick(0)).unwrap(),
        [Value::Ref(c)]
    );
    let reopened = World::open_read_only(&path).unwrap();
    assert_eq!(world.index_dump(), reopened.index_dump());
    assert_eq!(
        reopened.history(new_edge.into(), "related_to").unwrap(),
        expected
    );
    assert!(
        doctor(&reopened, &DoctorOptions::default())
            .findings
            .is_empty()
    );
}

#[test]
fn migration_preserves_committed_bidirectional_ref_edge_property_history() {
    migrate_with_mutual_edge_property(false);
}

#[test]
fn migration_preserves_staged_bidirectional_ref_edge_property_history() {
    migrate_with_mutual_edge_property(true);
}
