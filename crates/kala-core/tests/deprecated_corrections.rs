//! Deprecation stops new story facts, while corrections can still repair canon.
use kala_core::*;

#[test]
fn corrections_on_a_deprecated_attribute_commit_and_replay() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("world.kala");
    let mut world = World::create(
        &path,
        Schema::new().node_type("CHARACTER").attribute(AttrDef::new(
            "alias",
            ValueType::Str,
            Kind::State,
            Cardinality::Many,
        )),
    )
    .unwrap();
    let e = world.create_entity("CHARACTER", Some("wren")).unwrap();
    let mut ids = Vec::new();
    for (value, tick) in [
        ("deleted", -20),
        ("moved", -10),
        ("renamed", 0),
        ("ended", 10),
        ("vague", 20),
    ] {
        ids.push(
            world
                .assert(e, "alias", Value::Str(value.into()), Some(Tick(tick)))
                .unwrap()
                .entries[0],
        );
    }
    let before = world.latest_tx();
    let mut tx = world.begin();
    tx.deprecate_attribute("alias").unwrap();
    tx.commit().unwrap();
    let corrections = [
        Correction::Delete,
        Correction::Replace {
            value: None,
            tick: Some(Tick(i64::MIN)),
            op: None,
            precision: None,
        },
        Correction::Replace {
            value: Some(Value::Str("new name".into())),
            tick: None,
            op: None,
            precision: None,
        },
        Correction::Replace {
            value: None,
            tick: None,
            op: Some(Op::Retract),
            precision: None,
        },
        Correction::Replace {
            value: None,
            tick: None,
            op: None,
            precision: Some(Precision::Year),
        },
    ];
    for (entry, correction) in ids.into_iter().zip(corrections) {
        let corrected = world.correct(entry, correction);
        assert!(
            corrected.is_ok(),
            "corrections must remain writable after deprecation: {corrected:?}"
        );
        assert!(corrected.unwrap().changed > 0);
    }
    for result in [
        world.assert(e, "alias", Value::Str("x".into()), Some(Tick(i64::MAX))),
        world.retract(e, "alias", Value::Str("moved".into()), Some(Tick(i64::MAX))),
    ] {
        assert!(
            matches!(result, Err(Error::DeprecatedAttribute(_))),
            "{result:?}"
        );
    }
    let history = world.history(e, "alias").unwrap();
    assert_eq!(history.len(), 4);
    assert!(
        history
            .iter()
            .any(|c| c.tick == Tick(i64::MIN) && c.value == Value::Str("moved".into()))
    );
    assert!(
        history
            .iter()
            .any(|c| c.value == Value::Str("new name".into()))
    );
    assert!(
        history
            .iter()
            .any(|c| c.value == Value::Str("ended".into()) && c.op == Op::Retract)
    );
    assert!(history.iter().any(|c| c.precision == Precision::Year));
    assert_eq!(world.at_canon(before).history(e, "alias").unwrap().len(), 5);
    let ticks = [i64::MIN, -21, -20, -1, 0, 9, 10, 20, i64::MAX];
    let states: Vec<_> = ticks
        .iter()
        .map(|t| world.as_of(e, "alias", Tick(*t)).unwrap())
        .collect();
    assert_eq!(states[0], [Value::Str("moved".into())]);
    assert_eq!(
        states[8],
        [
            Value::Str("moved".into()),
            Value::Str("new name".into()),
            Value::Str("vague".into())
        ]
    );
    assert!(
        doctor(&world, &DoctorOptions::default())
            .findings
            .is_empty()
    );
    drop(world);
    let reopened = World::open_read_only(&path).unwrap();
    assert_eq!(reopened.history(e, "alias").unwrap(), history);
    for (t, state) in ticks.into_iter().zip(states) {
        assert_eq!(reopened.as_of(e, "alias", Tick(t)).unwrap(), state);
    }
    assert!(
        doctor(&reopened, &DoctorOptions::default())
            .findings
            .is_empty()
    );
}

#[test]
fn corrections_of_migrated_history_leave_the_replacement_untouched() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("world.kala");
    let mut world = World::create(
        &path,
        Schema::new().node_type("CHARACTER").attribute(AttrDef::new(
            "x",
            ValueType::I64,
            Kind::State,
            Cardinality::One,
        )),
    )
    .unwrap();
    let e = world.create_entity("CHARACTER", None).unwrap();
    let entry = world
        .assert(e, "x", Value::I64(7), Some(Tick(-10)))
        .unwrap()
        .entries[0];
    let mut tx = world.begin();
    tx.migrate_attribute(
        "x",
        MigrationOptions {
            value_type: Some(ValueType::Str),
            ..MigrationOptions::default()
        },
    )
    .unwrap();
    tx.commit().unwrap();
    let migration_canon = world.latest_tx();
    let correction = world.correct(
        entry,
        Correction::Replace {
            value: Some(Value::I64(8)),
            tick: Some(Tick(0)),
            op: None,
            precision: None,
        },
    );
    assert!(
        correction.is_ok(),
        "a migration's deprecated source remains correctable: {correction:?}"
    );
    assert!(world.as_of(e, "x@old", Tick(-1)).unwrap().is_empty());
    assert_eq!(world.as_of(e, "x@old", Tick(0)).unwrap(), [Value::I64(8)]);
    assert_eq!(
        world.as_of(e, "x", Tick(0)).unwrap(),
        [Value::Str("7".into())]
    );
    assert_eq!(
        world
            .at_canon(migration_canon)
            .as_of(e, "x@old", Tick(0))
            .unwrap(),
        [Value::I64(7)]
    );
    drop(world);
    let reopened = World::open_read_only(&path).unwrap();
    assert!(reopened.as_of(e, "x@old", Tick(-1)).unwrap().is_empty());
    assert_eq!(
        reopened.as_of(e, "x@old", Tick(0)).unwrap(),
        [Value::I64(8)]
    );
    assert_eq!(
        reopened.as_of(e, "x", Tick(0)).unwrap(),
        [Value::Str("7".into())]
    );
    assert!(
        doctor(&reopened, &DoctorOptions::default())
            .findings
            .is_empty()
    );
}
