//! Edge cases for the PR #6 polish fixes (migrations, reserved names, schema edits).

use kala_core::*;

fn mig(world: &mut World, name: &str, options: MigrationOptions) -> Result<MigrationReport, Error> {
    let mut tx = world.begin();
    let r = tx.migrate_attribute(name, options)?;
    tx.commit()?;
    Ok(r)
}

fn to_type(t: ValueType) -> MigrationOptions {
    MigrationOptions {
        value_type: Some(t),
        ..MigrationOptions::default()
    }
}

fn to_bidi() -> MigrationOptions {
    MigrationOptions {
        direction: Some(RefDirection::Bidirectional),
        ..MigrationOptions::default()
    }
}

// --- 1 ---------------------------------------------------------------------

#[test]
fn three_chained_edge_property_migrations_then_bidirectional() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("w.kala");
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
    for who in [edwin, mara] {
        world
            .assert(who, "present", Value::Ref(carnath), Some(Tick(-300)))
            .unwrap();
    }
    world
        .retract(edwin, "present", Value::Ref(carnath), Some(Tick(-250)))
        .unwrap();
    let e_edwin = world.edge_id(edwin, "present", carnath).unwrap();
    let e_mara = world.edge_id(mara, "present", carnath).unwrap();
    for (e, t, v) in [
        (e_edwin, -300, true),
        (e_edwin, -280, false),
        (e_mara, -299, true),
    ] {
        world
            .assert(e.into(), "disguised", Value::Bool(v), Some(Tick(t)))
            .unwrap();
    }
    world
        .retract(
            e_edwin.into(),
            "disguised",
            Value::Bool(true),
            Some(Tick(-280)),
        )
        .unwrap();

    // bool -> i64 -> f64 -> string, each a convertible step.
    mig(&mut world, "disguised", to_type(ValueType::I64)).expect("bool->i64");
    mig(&mut world, "disguised", to_type(ValueType::F64)).expect("i64->f64");
    mig(&mut world, "disguised", to_type(ValueType::Str)).expect("f64->string");
    for old in ["disguised@old", "disguised@old2", "disguised@old3"] {
        assert!(world.attribute(old).unwrap().deprecated, "{old}");
    }
    let r = mig(&mut world, "present", to_bidi()).expect("bidirectional with 4 property histories");
    assert!(r.edge_properties >= 4 * 3, "{r:?}");

    for who in [edwin, mara] {
        let old = if who == edwin { e_edwin } else { e_mara };
        let new = world.edge_id(who, "present", carnath).unwrap();
        assert_ne!(new, old);
        for prop in [
            "disguised",
            "disguised@old",
            "disguised@old2",
            "disguised@old3",
        ] {
            let h = world.history(old.into(), prop).unwrap();
            assert!(!h.is_empty(), "{prop} non-vacuous");
            assert_eq!(world.history(new.into(), prop).unwrap(), h, "{prop}");
        }
    }
    let reopened = World::open_read_only(&path).unwrap();
    assert_eq!(world.index_dump(), reopened.index_dump());
    let rep = doctor(&reopened, &DoctorOptions::default());
    assert!(rep.findings.is_empty(), "{:?}", rep.findings);
}

// --- 2 ---------------------------------------------------------------------

#[test]
fn migrate_with_explicit_inverse_and_bidirectional_is_refused() {
    let mut world = World::in_memory(Schema::new().node_type("CHARACTER").attribute(AttrDef::new(
        "x",
        ValueType::Ref,
        Kind::State,
        Cardinality::Many,
    )))
    .unwrap();
    let before = world.latest_tx();
    let err = mig(
        &mut world,
        "x",
        MigrationOptions {
            direction: Some(RefDirection::Bidirectional),
            inverse: Some("y".into()),
            ..MigrationOptions::default()
        },
    )
    .expect_err("explicit inverse + bidirectional must be refused");
    let _ = err;
    assert_eq!(world.latest_tx(), before);
    assert!(world.attribute("y").is_none());
    assert_eq!(
        world.attribute("x").unwrap().direction,
        Some(RefDirection::Directed)
    );
}

// --- 3 ---------------------------------------------------------------------

fn age_world() -> (World, EntityId) {
    let mut w = World::in_memory(Schema::new().node_type("CHARACTER")).unwrap();
    let e = w.create_entity("CHARACTER", Some("greg")).unwrap();
    (w, e)
}

#[test]
fn declare_facts_and_migrate_in_one_transaction() {
    let path_dir = tempfile::tempdir().unwrap();
    let path = path_dir.path().join("w.kala");
    let mut world = World::create(&path, Schema::new().node_type("CHARACTER")).unwrap();
    let greg = world.create_entity("CHARACTER", Some("greg")).unwrap();
    let mut tx = world.begin();
    tx.declare_attribute(AttrDef::new(
        "rank",
        ValueType::I64,
        Kind::State,
        Cardinality::One,
    ))
    .unwrap();
    tx.assert(greg, "rank", Value::I64(1), Some(Tick(0)))
        .unwrap();
    tx.assert(greg, "rank", Value::I64(2), Some(Tick(5)))
        .unwrap(); // overlap, never retracted
    tx.migrate_attribute("rank", to_type(ValueType::F64))
        .unwrap();
    let one = tx.commit().unwrap();
    assert!(world.attribute("rank@old").unwrap().deprecated);
    assert_eq!(world.attribute("rank").unwrap().value_type, ValueType::F64);
    assert_eq!(world.history(greg, "rank").unwrap().len(), 2);

    // The same steps in two transactions leave the same overlap.
    let (mut w2, g2) = age_world();
    let mut tx = w2.begin();
    tx.declare_attribute(AttrDef::new(
        "rank",
        ValueType::I64,
        Kind::State,
        Cardinality::One,
    ))
    .unwrap();
    tx.assert(g2, "rank", Value::I64(1), Some(Tick(0))).unwrap();
    tx.assert(g2, "rank", Value::I64(2), Some(Tick(5))).unwrap();
    let first = tx.commit().unwrap();
    let mut tx = w2.begin();
    tx.migrate_attribute("rank", to_type(ValueType::F64))
        .unwrap();
    let second = tx.commit().unwrap();
    // One txn: both overlaps that exist at the end (live and @old) are new.
    let mut names: Vec<&str> = one
        .warnings
        .introduced
        .iter()
        .map(|o| o.attr.as_str())
        .collect();
    names.sort();
    assert_eq!(names, ["rank", "rank@old"], "{:?}", one.warnings);
    assert!(one.warnings.resolved.is_empty(), "{:?}", one.warnings);
    // Two txns: the same two overlaps are introduced across the two commits.
    let mut names: Vec<(&str, &Value)> = first
        .warnings
        .introduced
        .iter()
        .chain(&second.warnings.introduced)
        .map(|o| (o.attr.as_str(), &o.a))
        .collect();
    names.sort_by_key(|(n, _)| n.to_string());
    assert_eq!(
        names.len(),
        2,
        "{:?} / {:?}",
        first.warnings,
        second.warnings
    );
    // Both end states have the same overlaps.
    assert_eq!(
        world.overlaps("rank").unwrap(),
        w2.overlaps("rank").unwrap()
    );
    assert_eq!(
        world.overlaps("rank@old").unwrap().len(),
        w2.overlaps("rank@old").unwrap().len()
    );

    let reopened = World::open_read_only(&path).unwrap();
    assert_eq!(world.index_dump(), reopened.index_dump());
    assert!(
        doctor(&reopened, &DoctorOptions::default())
            .findings
            .iter()
            .all(|f| {
                // cardinality overlap is the only expected warning here
                format!("{f:?}").to_lowercase().contains("overlap")
                    || format!("{f:?}").contains("cardinality")
            })
    );
    assert_eq!(reopened.overlaps("rank").unwrap().len(), 1);
}

// --- 4 ---------------------------------------------------------------------

#[test]
fn reserved_inverse_name_is_refused_and_poisons_the_transaction() {
    let mut world = World::in_memory(Schema::new().node_type("CHARACTER")).unwrap();
    let greg = world.create_entity("CHARACTER", Some("greg")).unwrap();
    let before = world.latest_tx();
    {
        let mut tx = world.begin();
        let err = tx
            .declare_attribute(
                AttrDef::new("x", ValueType::Ref, Kind::State, Cardinality::Many)
                    .inverse("_KALA_x"),
            )
            .unwrap_err();
        assert!(err.to_string().contains("_KALA_"), "{err}");
        assert!(err.to_string().to_lowercase().contains("reserved"), "{err}");
        let next = tx.retract(greg, "nope", Value::I64(1), Some(Tick(0)));
        assert!(matches!(next, Err(Error::TransactionAborted)), "{next:?}");
        assert_eq!(tx.commit(), Err(Error::TransactionAborted));
    }
    assert_eq!(world.latest_tx(), before);
    assert!(world.attribute("x").is_none());
}

#[test]
fn only_the_exact_reserved_prefix_is_reserved() {
    let mut world = World::in_memory(Schema::new().node_type("CHARACTER")).unwrap();
    let mut tx = world.begin();
    tx.declare_attribute(AttrDef::new(
        "_kala_x",
        ValueType::Str,
        Kind::State,
        Cardinality::One,
    ))
    .unwrap();
    tx.declare_node_type("_kala_t").unwrap();
    tx.declare_attribute(
        AttrDef::new("rel", ValueType::Ref, Kind::State, Cardinality::Many).inverse("_Kala_rel"),
    )
    .unwrap();
    tx.commit().unwrap();
    assert!(world.attribute("_kala_x").is_some());
}

#[test]
fn genesis_system_world_reopens_read_write_and_accepts_native_writes() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("system.kala");
    let schema = Schema::new()
        .node_type("_KALA_SYSTEM")
        .node_type("ACCOUNT")
        .attribute(AttrDef::new(
            "login",
            ValueType::Str,
            Kind::State,
            Cardinality::One,
        ));
    drop(World::create(&path, schema).unwrap());
    let mut w = World::open(&path).expect("reopen read-write");
    let acct = w.create_entity("ACCOUNT", Some("alice")).unwrap();
    w.assert(acct, "login", Value::Str("alice".into()), Some(Tick(0)))
        .unwrap();
    let sys = w.create_entity("_KALA_SYSTEM", Some("sys")).unwrap();
    assert!(w.history(sys, "login").unwrap().is_empty());
    drop(w);
    let again = World::open(&path).expect("and once more after writing");
    assert_eq!(again.history(acct, "login").unwrap().len(), 1);
    assert!(
        doctor(&again, &DoctorOptions::default())
            .findings
            .is_empty()
    );
}

// --- 5 ---------------------------------------------------------------------

#[test]
fn attribute_form_on_inferred_attribute_is_declared_then_cardinality_in_one_txn_replays() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("w.kala");
    let mut world = World::create(&path, Schema::new().node_type("CHARACTER")).unwrap();
    let greg = world.create_entity("CHARACTER", Some("greg")).unwrap();
    let mut tx = world.begin();
    tx.infer_attribute("age", &Value::I64(1)).unwrap();
    tx.assert(greg, "age", Value::I64(1), Some(Tick(0)))
        .unwrap();
    tx.commit().unwrap();
    assert!(world.attribute("age").unwrap().inferred);

    let mut tx = world.begin();
    tx.declare_attribute_with(
        "age",
        ValueType::I64,
        &AttributeOptions {
            cardinality: Some(Cardinality::Many),
            ..AttributeOptions::default()
        },
    )
    .unwrap();
    let res = tx.commit().unwrap();
    assert_eq!(
        res.schema.len(),
        2,
        "declared + cardinality: {:?}",
        res.schema
    );
    let def = world.attribute("age").unwrap();
    assert!(!def.inferred);
    assert_eq!(def.cardinality, Cardinality::Many);
    let reopened = World::open_read_only(&path).unwrap();
    assert_eq!(world.index_dump(), reopened.index_dump());
    assert_eq!(
        reopened.attribute("age").unwrap().cardinality,
        Cardinality::Many
    );
}

#[test]
fn attribute_form_on_deprecated_attribute_is_refused_with_deprecated_error() {
    let mut world = World::in_memory(Schema::new().node_type("CHARACTER").attribute(AttrDef::new(
        "old",
        ValueType::Str,
        Kind::State,
        Cardinality::One,
    )))
    .unwrap();
    let mut tx = world.begin();
    tx.deprecate_attribute("old").unwrap();
    tx.commit().unwrap();
    let mut tx = world.begin();
    let err = tx
        .declare_attribute_with("old", ValueType::Str, &AttributeOptions::default())
        .unwrap_err();
    assert!(
        matches!(err, Error::DeprecatedAttribute(ref n) if n == "old")
            || err.to_string().to_lowercase().contains("deprecated"),
        "{err:?}"
    );
}

#[test]
fn attribute_form_with_new_type_on_factless_attribute_starts_from_defaults() {
    let mut world = World::in_memory(Schema::new().node_type("CHARACTER").attribute(AttrDef::new(
        "x",
        ValueType::Str,
        Kind::Event,
        Cardinality::Many,
    )))
    .unwrap();
    let mut tx = world.begin();
    tx.declare_attribute_with("x", ValueType::I64, &AttributeOptions::default())
        .unwrap();
    tx.commit().unwrap();
    let def = world.attribute("x").unwrap();
    assert_eq!(def.value_type, ValueType::I64);
    assert_eq!(
        def.kind,
        Kind::State,
        "kind from defaults, not the old Event"
    );
    assert_eq!(
        def.cardinality,
        Cardinality::One,
        "cardinality from defaults"
    );
    assert!(def.indexed);
}

// --- 8 ---------------------------------------------------------------------

#[test]
fn migration_with_dropped_inverse_and_edge_properties_is_deterministic_on_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("w.kala");
    let mut world = World::create(
        &path,
        Schema::new()
            .node_type("CHARACTER")
            .attribute(
                AttrDef::new("parent_of", ValueType::Ref, Kind::State, Cardinality::Many)
                    .inverse("child_of")
                    .reverse_one(),
            )
            .attribute(AttrDef::new(
                "note",
                ValueType::Str,
                Kind::State,
                Cardinality::One,
            )),
    )
    .unwrap();
    let a = world.create_entity("CHARACTER", Some("a")).unwrap();
    let b = world.create_entity("CHARACTER", Some("b")).unwrap();
    let c = world.create_entity("CHARACTER", Some("c")).unwrap();
    for (s, t) in [(a, b), (a, c)] {
        world
            .assert(s, "parent_of", Value::Ref(t), Some(Tick(0)))
            .unwrap();
    }
    let e = world.edge_id(a, "parent_of", b).unwrap();
    world
        .assert(e.into(), "note", Value::Str("one".into()), Some(Tick(1)))
        .unwrap();
    world
        .retract(e.into(), "note", Value::Str("one".into()), Some(Tick(2)))
        .unwrap();
    world
        .assert(e.into(), "note", Value::Str("two".into()), Some(Tick(2)))
        .unwrap();
    world
        .retract(a, "parent_of", Value::Ref(b), Some(Tick(9)))
        .unwrap();
    let r = mig(&mut world, "parent_of", to_bidi()).unwrap();
    assert_eq!(r.dropped_inverse.as_deref(), Some("child_of"));
    assert!(r.edge_properties >= 2, "{r:?}");
    let ne = world.edge_id(a, "parent_of", b).unwrap();
    assert_eq!(world.history(ne.into(), "note").unwrap().len(), 3);
    for _ in 0..2 {
        let reopened = World::open_read_only(&path).unwrap();
        assert_eq!(world.index_dump(), reopened.index_dump());
        assert_eq!(reopened.edge_id(a, "parent_of", b), Some(ne));
        let rep = doctor(&reopened, &DoctorOptions::default());
        assert!(rep.findings.is_empty(), "{:?}", rep.findings);
    }
}
