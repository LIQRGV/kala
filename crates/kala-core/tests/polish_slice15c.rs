//! PR #21, second polish: a transaction limited to schema additions
//! (`Txn::set_schema_additions_only`), and world-setting keys at genesis
//! and in a restore.

use kala_core::doctor::{DoctorOptions, doctor};
use kala_core::*;

fn schema() -> Schema {
    let attr = AttrDef::new;
    Schema::new()
        .node_type("CHARACTER")
        .node_type("PLACE")
        .attribute(attr("located_in", ValueType::Ref, Kind::State, Cardinality::One).directed())
        .attribute(attr("name", ValueType::Str, Kind::State, Cardinality::One))
}

fn many() -> AttributeOptions {
    AttributeOptions {
        cardinality: Some(Cardinality::Many),
        ..AttributeOptions::default()
    }
}

fn refused(result: Result<(), Error>, name: &str) {
    assert_eq!(
        result,
        Err(Error::SchemaChangeNotPermitted(name.to_owned())),
        "{name}"
    );
}

#[test]
fn an_additions_only_transaction_cannot_change_what_the_committed_schema_has() {
    type Edit = fn(&mut Txn<'_>) -> Result<(), Error>;
    let edits: [(&str, Edit); 8] = [
        ("located_in", |t| {
            t.declare_attribute_with("located_in", ValueType::Ref, &many())
        }),
        ("located_in", |t| {
            t.declare_attribute_with(
                "located_in",
                ValueType::Ref,
                &AttributeOptions {
                    inverse: Some("location_of".into()),
                    ..AttributeOptions::default()
                },
            )
        }),
        // No facts yet, so the store itself would let the type change.
        ("name", |t| {
            t.declare_attribute_with("name", ValueType::I64, &AttributeOptions::default())
        }),
        ("name", |t| t.set_cardinality("name", Cardinality::Many)),
        ("name", |t| t.rename_attribute("name", "title")),
        ("name", |t| t.deprecate_attribute("name")),
        ("name", |t| {
            t.migrate_attribute(
                "name",
                MigrationOptions {
                    cardinality: Some(Cardinality::Many),
                    ..MigrationOptions::default()
                },
            )
            .map(|_| ())
        }),
        ("tick_unit", |t| t.world_setting("tick_unit", "day")),
    ];
    for (i, (name, edit)) in edits.into_iter().enumerate() {
        let mut w = World::in_memory(schema()).unwrap();
        let before = w.entries().count();
        let mut t = w.begin();
        t.set_schema_additions_only(true);
        refused(edit(&mut t), name);
        // Like every failed write, it poisons the transaction.
        assert_eq!(t.commit().unwrap_err(), Error::TransactionAborted, "{i}");
        assert_eq!(w.entries().count(), before, "{i}");
        // Without the limit the same edit is an ordinary schema change.
        let mut t = w.begin();
        edit(&mut t).unwrap();
        assert!(t.commit().unwrap().declared > 0, "{i}");
    }
}

#[test]
fn an_additions_only_transaction_declares_new_names_and_repeats_existing_ones() {
    let mut w = World::in_memory(schema()).unwrap();
    let mut t = w.begin();
    t.set_schema_additions_only(true);
    // Exactly what exists: no edit.
    t.declare_attribute_with(
        "located_in",
        ValueType::Ref,
        &AttributeOptions {
            cardinality: Some(Cardinality::One),
            ..AttributeOptions::default()
        },
    )
    .unwrap();
    t.declare_node_type("CHARACTER").unwrap();
    assert!(t.schema_changes().is_empty());
    // New names, and a second thought about one of its own.
    t.declare_node_type("FACTION").unwrap();
    t.declare_attribute_with("age", ValueType::I64, &AttributeOptions::default())
        .unwrap();
    t.declare_attribute_with("age", ValueType::I64, &many())
        .unwrap();
    let greg = t.create_entity("CHARACTER", Some("greg")).unwrap();
    // Inference adds too.
    t.write_inferring(
        greg,
        "title",
        &Value::Str("Sir".into()),
        Some(Tick(1)),
        Op::Assert,
        Precision::Exact,
    )
    .unwrap();
    let r = t.commit().unwrap();
    assert!(r.committed);
    assert_eq!(w.attribute("age").unwrap().cardinality, Cardinality::Many);
    assert!(w.attribute("title").unwrap().inferred);
    assert!(doctor(&w, &DoctorOptions::default()).is_clean());
    // Once committed, they are the committed schema: confirming the
    // inferred attribute as declared is a change of it.
    let mut t = w.begin();
    t.set_schema_additions_only(true);
    refused(
        t.declare_attribute_with("title", ValueType::Str, &AttributeOptions::default()),
        "title",
    );
}

#[test]
fn a_name_another_commit_declared_meanwhile_is_refused_at_commit() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("world.kala");
    let mut a = World::create(&path, schema()).unwrap();
    let mut b = World::open(&path).unwrap();
    for weak in [false, true] {
        let name = if weak { "rank" } else { "age" };
        let mut t = b.begin();
        t.set_schema_additions_only(true);
        if weak {
            t.set_concurrency(Concurrency::Weak);
        }
        // New here...
        t.declare_attribute_with(name, ValueType::I64, &many())
            .unwrap();
        // ...but committed, with other labels, before this commits.
        let mut ta = a.begin();
        ta.declare_attribute_with(name, ValueType::I64, &AttributeOptions::default())
            .unwrap();
        ta.commit().unwrap();
        let before = std::fs::read(&path).unwrap();
        refused(t.commit().map(|_| ()), name);
        assert_eq!(std::fs::read(&path).unwrap(), before, "nothing written");
        assert_eq!(b.attribute(name).unwrap().cardinality, Cardinality::One);
    }
}

// ------------------------------------------------------- world-setting keys

fn known_keys(error: &Error) {
    let Error::Schema(message) = error else {
        panic!("{error:?}")
    };
    assert!(message.contains("unknown world setting"), "{message}");
    for known in WORLD_SETTING_KEYS {
        assert!(message.contains(&format!(":{known}")), "{message}");
    }
}

#[test]
fn a_genesis_setting_with_an_unknown_key_is_refused_like_a_live_one() {
    let genesis = World::in_memory(Schema::new().setting("house_rule", "x")).unwrap_err();
    known_keys(&genesis);
    let mut w = World::in_memory(Schema::new()).unwrap();
    let live = w.begin().world_setting("house_rule", "x").unwrap_err();
    assert_eq!(genesis, live);
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("w.kala");
    known_keys(&World::create(&path, Schema::new().setting("house_rule", "x")).unwrap_err());
    assert!(!path.exists(), "nothing was created");
    for (key, value) in [
        ("schema", "strict"),
        ("concurrency", "optimistic"),
        ("tick_unit", "day"),
    ] {
        assert!(World::in_memory(Schema::new().setting(key, value)).is_ok());
    }
}

#[test]
fn a_restore_may_write_unknown_setting_keys_at_genesis_and_later() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("w.kala");
    let mut w = World::create(
        &path,
        Schema::new()
            .setting("house_rule", "x")
            .accepting_unknown_settings(),
    )
    .unwrap();
    let mut t = w.begin();
    known_keys(&t.world_setting("old_key", "1").unwrap_err());
    drop(t);
    let mut t = w.begin();
    t.set_unknown_settings_allowed(true);
    t.world_setting("old_key", "1").unwrap();
    // Values are still validated.
    assert!(t.world_setting("schema", "loose").is_err());
    drop(t);
    let mut t = w.begin();
    t.set_unknown_settings_allowed(true);
    t.world_setting("old_key", "1").unwrap();
    t.commit().unwrap();
    drop(w);
    let w = World::open(&path).unwrap();
    assert_eq!(w.world_setting("house_rule"), Some("x"));
    assert_eq!(w.world_setting("old_key"), Some("1"));
    assert!(doctor(&w, &DoctorOptions::default()).is_clean());
}
