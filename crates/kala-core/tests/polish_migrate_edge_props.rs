//! Migrating a relationship copies every edge property history, including the
//! deprecated `@old` properties left by an earlier migration.

use kala_core::*;

struct Fx {
    _dir: tempfile::TempDir,
    path: std::path::PathBuf,
    world: World,
    old_edges: Vec<(EntityId, EntityId, EdgeId)>,
}

fn build() -> Fx {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("world.kala");
    let mut world = World::create(
        &path,
        Schema::new()
            .node_type("CHARACTER")
            .node_type("PLACE")
            .attribute(AttrDef::new(
                "present",
                ValueType::Ref,
                Kind::State,
                Cardinality::Many,
            ))
            .attribute(AttrDef::new(
                "disguised",
                ValueType::Bool,
                Kind::State,
                Cardinality::One,
            )),
    )
    .unwrap();
    let edwin = world.create_entity("CHARACTER", Some("edwin")).unwrap();
    let mara = world.create_entity("CHARACTER", Some("mara")).unwrap();
    let carnath = world.create_entity("PLACE", Some("carnath")).unwrap();
    let varrow = world.create_entity("PLACE", Some("varrow")).unwrap();
    let mut tx = world.begin();
    for (who, place) in [(edwin, carnath), (mara, carnath), (edwin, varrow)] {
        tx.assert(who, "present", Value::Ref(place), Some(Tick(-300)))
            .unwrap();
    }
    tx.retract(edwin, "present", Value::Ref(carnath), Some(Tick(-250)))
        .unwrap();
    tx.commit().unwrap();
    let mut old_edges = Vec::new();
    for (who, place) in [(edwin, carnath), (mara, carnath), (edwin, varrow)] {
        old_edges.push((who, place, world.edge_id(who, "present", place).unwrap()));
    }
    let mut tx = world.begin();
    let e0 = old_edges[0].2.into();
    let e1 = old_edges[1].2.into();
    tx.assert_with_precision(
        e0,
        "disguised",
        Value::Bool(true),
        Some(Tick(-300)),
        Precision::Year,
    )
    .unwrap();
    tx.assert(e0, "disguised", Value::Bool(false), Some(Tick(-280)))
        .unwrap();
    tx.retract_with_precision(
        e0,
        "disguised",
        Value::Bool(true),
        Some(Tick(-280)),
        Precision::Day,
    )
    .unwrap();
    tx.assert(e1, "disguised", Value::Bool(true), Some(Tick(-299)))
        .unwrap();
    tx.commit().unwrap();
    Fx {
        _dir: dir,
        path,
        world,
        old_edges,
    }
}

fn migrate(
    world: &mut World,
    name: &str,
    options: MigrationOptions,
) -> Result<MigrationReport, Error> {
    let mut tx = world.begin();
    let report = tx.migrate_attribute(name, options)?;
    tx.commit()?;
    Ok(report)
}

fn both_migrations(f: &mut Fx) -> Result<MigrationReport, Error> {
    migrate(
        &mut f.world,
        "disguised",
        MigrationOptions {
            value_type: Some(ValueType::Str),
            ..MigrationOptions::default()
        },
    )
    .expect("first migration (bool -> string) must succeed");
    migrate(
        &mut f.world,
        "present",
        MigrationOptions {
            direction: Some(RefDirection::Bidirectional),
            ..MigrationOptions::default()
        },
    )
}

#[test]
fn second_migration_copies_both_new_and_deprecated_edge_properties() {
    let mut f = build();
    let report = both_migrations(&mut f).expect(
        "migrating a relationship must not trip over the deprecated `disguised@old` property",
    );
    assert!(report.edge_properties > 0, "{report:?}");
    for (who, place, old) in f.old_edges.clone() {
        let new = f
            .world
            .edge_id(who, "present", place)
            .expect("new edge exists for every old edge");
        assert_ne!(new, old);
        for prop in ["disguised", "disguised@old"] {
            assert_eq!(
                f.world.history(new.into(), prop).unwrap(),
                f.world.history(old.into(), prop).unwrap(),
                "{prop} history of {who:?} -> {place:?} must be copied point for point"
            );
        }
    }
    // The copies are not vacuous: the first edge has both histories.
    let (who, place, _) = f.old_edges[0];
    let new = f.world.edge_id(who, "present", place).unwrap();
    assert_eq!(f.world.history(new.into(), "disguised").unwrap().len(), 3);
    assert_eq!(
        f.world.history(new.into(), "disguised@old").unwrap().len(),
        3
    );
}

#[test]
fn reopening_after_both_migrations_reads_the_same_histories_and_doctor_is_clean() {
    let mut f = build();
    both_migrations(&mut f).expect("both migrations succeed");
    let reopened = World::open_read_only(&f.path).expect("replay of the migrated log");
    assert_eq!(f.world.index_dump(), reopened.index_dump());
    for (who, place, old) in f.old_edges.clone() {
        let new = f.world.edge_id(who, "present", place).unwrap();
        assert_eq!(reopened.edge_id(who, "present", place), Some(new));
        for edge in [new, old] {
            for prop in ["disguised", "disguised@old"] {
                assert_eq!(
                    reopened.history(edge.into(), prop).unwrap(),
                    f.world.history(edge.into(), prop).unwrap(),
                    "{prop} on {edge:?}"
                );
            }
        }
    }
    let report = doctor(&reopened, &DoctorOptions::default());
    assert!(report.findings.is_empty(), "{:?}", report.findings);
}

/// Guard: passes today and must keep passing.
#[test]
fn deprecated_edge_property_still_refuses_user_writes_after_migrations() {
    let mut f = build();
    // Today the second migration fails; only the first is needed for the guard.
    migrate(
        &mut f.world,
        "disguised",
        MigrationOptions {
            value_type: Some(ValueType::Str),
            ..MigrationOptions::default()
        },
    )
    .unwrap();
    let _ = migrate(
        &mut f.world,
        "present",
        MigrationOptions {
            direction: Some(RefDirection::Bidirectional),
            ..MigrationOptions::default()
        },
    );
    let (who, place, _) = f.old_edges[0];
    let edge = f.world.edge_id(who, "present", place).unwrap();
    let err = f
        .world
        .assert(
            edge.into(),
            "disguised@old",
            Value::Bool(true),
            Some(Tick(0)),
        )
        .unwrap_err();
    assert!(
        matches!(err, Error::DeprecatedAttribute(ref n) if n == "disguised@old"),
        "{err:?}"
    );
}
