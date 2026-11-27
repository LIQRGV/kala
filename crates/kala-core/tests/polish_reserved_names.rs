//! `kala_` is reserved everywhere. `_KALA_` is reserved after genesis;
//! the system world's genesis may use that prefix.

mod common;

use common::{commit_frame, frame, txn};
use kala_core::log::read_log;
use kala_core::*;

fn schema() -> Schema {
    Schema::new().node_type("CHARACTER").attribute(AttrDef::new(
        "title",
        ValueType::Str,
        Kind::State,
        Cardinality::One,
    ))
}

fn assert_reserved<T: std::fmt::Debug>(what: &str, result: Result<T, Error>) {
    match result {
        Ok(v) => panic!("{what}: must be refused as reserved, got Ok({v:?})"),
        Err(e) => {
            let text = e.to_string();
            assert!(
                text.contains("_KALA_"),
                "{what}: error must name the reserved `_KALA_` prefix: {text}"
            );
            assert!(text.to_lowercase().contains("reserved"), "{what}: {text}");
        }
    }
}

#[test]
fn inference_of_reserved_node_type_and_attribute_is_refused() {
    let mut world = World::in_memory(schema()).unwrap();
    // A refusal poisons its transaction, so each refused call gets its own.
    let mut tx = world.begin();
    assert_reserved("infer node type", tx.infer_node_type("_KALA_SYSTEM"));
    drop(tx);
    let mut tx = world.begin();
    assert_reserved(
        "infer attribute",
        tx.infer_attribute("_KALA_foo", &Value::I64(1)),
    );
}

#[test]
fn explicit_declaration_of_reserved_names_is_refused() {
    let mut world = World::in_memory(schema()).unwrap();
    // A refusal poisons its transaction, so each refused call gets its own.
    let mut tx = world.begin();
    assert_reserved("declare node type", tx.declare_node_type("_KALA_SYSTEM"));
    drop(tx);
    let mut tx = world.begin();
    assert_reserved(
        "declare attribute",
        tx.declare_attribute(AttrDef::new(
            "_KALA_x",
            ValueType::Str,
            Kind::State,
            Cardinality::One,
        )),
    );
}

#[test]
fn a_reserved_name_refusal_poisons_the_transaction() {
    let mut world = World::in_memory(schema()).unwrap();
    let before = world.latest_tx();
    {
        let mut tx = world.begin();
        assert_reserved("declare node type", tx.declare_node_type("_KALA_SYSTEM"));
        let valid = tx.create_entity("CHARACTER", Some("edwin"));
        assert!(
            matches!(valid, Err(Error::TransactionAborted)),
            "a valid write after a reserved refusal must be TransactionAborted: {valid:?}"
        );
        assert_eq!(tx.commit(), Err(Error::TransactionAborted));
    }
    assert_eq!(world.latest_tx(), before);
}

#[test]
fn rename_to_a_reserved_name_is_refused() {
    let mut world = World::in_memory(schema()).unwrap();
    let mut tx = world.begin();
    assert_reserved("rename", tx.rename_attribute("title", "_KALA_x"));
}

#[test]
fn refused_reserved_names_leave_the_world_unchanged_and_ordinary_names_work() {
    let mut world = World::in_memory(schema()).unwrap();
    let before = world.latest_tx();
    {
        let mut tx = world.begin();
        let _ = tx.infer_node_type("_KALA_SYSTEM");
        let _ = tx.declare_attribute(AttrDef::new(
            "_KALA_x",
            ValueType::Str,
            Kind::State,
            Cardinality::One,
        ));
        let _ = tx.commit();
    }
    assert_eq!(world.latest_tx(), before);
    assert!(!world.node_types().any(|n| n == "_KALA_SYSTEM"));
    assert!(world.attribute("_KALA_x").is_none());
    let mut tx = world.begin();
    tx.declare_node_type("_KALA").unwrap(); // not the reserved prefix
    tx.declare_node_type("KALA_X").unwrap();
    tx.commit().unwrap();
}

/// Guard: passes today. A genesis schema may carry the system marker.
#[test]
fn genesis_schema_may_still_carry_a_kala_system_node_type() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("system.kala");
    let world = World::create(
        &path,
        Schema::new().node_type("_KALA_SYSTEM").node_type("ACCOUNT"),
    )
    .expect("genesis with the system marker");
    assert!(world.node_types().any(|n| n == "_KALA_SYSTEM"));
    drop(world);
    let reopened = World::open_read_only(&path).expect("reopen a system-style world");
    assert!(reopened.node_types().any(|n| n == "_KALA_SYSTEM"));
}

// --- crafted logs -------------------------------------------------------

fn string(out: &mut Vec<u8>, text: &str) {
    out.extend_from_slice(&(text.len() as u32).to_le_bytes());
    out.extend_from_slice(text.as_bytes());
}

fn attribute(id: u32, name: &str, value_type: u8) -> Vec<u8> {
    let mut out = vec![0];
    out.extend_from_slice(&id.to_le_bytes());
    string(&mut out, name);
    out.extend_from_slice(&[value_type, 0, 0, 1, 0, 0, 0, 0, 0]);
    out
}

fn node_type(id: u16, name: &str) -> Vec<u8> {
    let mut out = vec![1];
    out.extend_from_slice(&id.to_le_bytes());
    string(&mut out, name);
    out.push(0);
    out
}

fn open_appended(entry: Vec<u8>) -> Result<World, Error> {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("world.kala");
    drop(World::create(&path, schema()).unwrap());
    let id = read_log(&path).unwrap().last().unwrap().id.0 + 1;
    let mut bytes = std::fs::read(&path).unwrap();
    let mut body = frame(9, id, &entry);
    body.extend_from_slice(&commit_frame(common::link(&bytes), &body, id + 1, 1));
    bytes.extend_from_slice(&txn(1, &body));
    std::fs::write(&path, bytes).unwrap();
    World::open_read_only(&path)
}

#[test]
fn crafted_schema_edit_introducing_a_reserved_name_is_corrupt_with_controls() {
    // Positive controls: identical shapes with an ordinary name open.
    assert!(open_appended(node_type(1, "FACTION")).is_ok());
    assert!(open_appended(attribute(1, "height", 3)).is_ok());
    for (what, entry) in [
        ("node type", node_type(1, "_KALA_SYSTEM")),
        ("attribute", attribute(1, "_KALA_height", 3)),
    ] {
        let opened = open_appended(entry);
        assert!(
            matches!(opened, Err(Error::Corrupt(_))),
            "{what}: a live write could not have produced this: {opened:?}"
        );
    }
}

fn assert_kala_reserved<T: std::fmt::Debug>(result: Result<T, Error>) {
    assert!(
        matches!(&result, Err(Error::ReservedName(_))),
        "expected ReservedName, got {result:?}"
    );
    let message = result.unwrap_err().to_string();
    assert!(
        message.contains("kala_") && message.contains("_KALA_"),
        "{message}"
    );
}

#[test]
fn kala_prefix_is_refused_by_every_live_name_introduction() {
    let mut world = World::in_memory(schema()).unwrap();
    assert_kala_reserved(world.begin().declare_node_type("kala_T"));
    assert_kala_reserved(world.begin().infer_node_type("kala_T"));
    assert_kala_reserved(world.begin().declare_attribute(AttrDef::new(
        "kala_x",
        ValueType::Str,
        Kind::State,
        Cardinality::One,
    )));
    assert_kala_reserved(world.begin().infer_attribute("kala_x", &Value::I64(1)));
    assert_kala_reserved(world.begin().rename_attribute("title", "kala_name"));
    assert_kala_reserved(world.begin().declare_attribute(
        AttrDef::new("relation", ValueType::Ref, Kind::State, Cardinality::Many).inverse("kala_y"),
    ));
    let mut tx = world.begin();
    tx.declare_attribute(AttrDef::new(
        "relation",
        ValueType::Ref,
        Kind::State,
        Cardinality::Many,
    ))
    .unwrap();
    tx.commit().unwrap();
    assert_kala_reserved(world.begin().migrate_attribute(
        "relation",
        MigrationOptions {
            inverse: Some("kala_inverse".into()),
            ..MigrationOptions::default()
        },
    ));
    let mut tx = world.begin();
    for name in ["kala", "Kala_T", "KALA_T", "handle"] {
        tx.declare_node_type(name).unwrap();
        tx.declare_attribute(AttrDef::new(
            name,
            ValueType::I64,
            Kind::State,
            Cardinality::One,
        ))
        .unwrap();
    }
    tx.commit().unwrap();
}

#[test]
fn kala_prefix_is_refused_at_genesis_for_types_attributes_and_inverses() {
    for schema in [
        Schema::new().node_type("kala_T"),
        schema().attribute(AttrDef::new(
            "kala_x",
            ValueType::Str,
            Kind::State,
            Cardinality::One,
        )),
        schema().attribute(
            AttrDef::new("relation", ValueType::Ref, Kind::State, Cardinality::Many)
                .inverse("kala_y"),
        ),
    ] {
        assert_kala_reserved(World::in_memory(schema));
    }
}

fn relation(id: u32, name: &str, inverse: &str) -> Vec<u8> {
    let mut out = vec![0];
    out.extend_from_slice(&id.to_le_bytes());
    string(&mut out, name);
    out.extend_from_slice(&[5, 0, 1, 1, 0, 1, 1]);
    string(&mut out, inverse);
    out.extend_from_slice(&[0, 0]);
    out
}

#[test]
fn crafted_kala_prefix_schema_edits_are_corrupt_with_positive_controls() {
    assert!(open_appended(node_type(1, "Kala_T")).is_ok());
    assert!(open_appended(attribute(1, "Kala_x", 4)).is_ok());
    assert!(open_appended(relation(1, "relation", "Kala_y")).is_ok());
    for entry in [
        node_type(1, "kala_T"),
        attribute(1, "kala_x", 4),
        attribute(0, "kala_title", 4),
        relation(1, "relation", "kala_y"),
    ] {
        let opened = open_appended(entry);
        assert!(matches!(&opened, Err(Error::Corrupt(_))), "{opened:?}");
        assert!(opened.unwrap_err().to_string().contains("kala_"));
    }
}

fn open_genesis(entries: Vec<(u8, Vec<u8>)>) -> Result<World, Error> {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("world.kala");
    let mut bytes = common::header(kala_core::log::LOG_FORMAT_VERSION);
    let mut body = Vec::new();
    for (i, (kind, payload)) in entries.iter().enumerate() {
        body.extend_from_slice(&frame(*kind, i as u64 + 1, payload));
    }
    body.extend_from_slice(&commit_frame(
        common::link(&bytes),
        &body,
        entries.len() as u64 + 1,
        0,
    ));
    bytes.extend_from_slice(&txn(0, &body));
    std::fs::write(&path, bytes).unwrap();
    World::open_read_only(&path)
}

#[test]
fn crafted_kala_prefix_genesis_is_corrupt_with_positive_controls() {
    for (name, reserved) in [("kala_T", true), ("Kala_T", false)] {
        let nt = node_type(0, name);
        let opened = open_genesis(vec![(1, nt[1..nt.len() - 1].to_vec())]);
        if reserved {
            assert!(matches!(opened, Err(Error::Corrupt(_))), "{opened:?}");
        } else {
            assert!(opened.is_ok(), "{opened:?}");
        }
    }
    for (name, inverse, reserved) in [
        ("kala_x", "ordinary", true),
        ("ordinary", "kala_y", true),
        ("ordinary", "Kala_y", false),
    ] {
        let payload = relation(0, name, inverse);
        let opened = open_genesis(vec![(2, payload[1..].to_vec())]);
        if reserved {
            assert!(matches!(opened, Err(Error::Corrupt(_))), "{opened:?}");
        } else {
            assert!(opened.is_ok(), "{opened:?}");
        }
    }
}

#[test]
fn handle_and_edge_are_ordinary_attribute_and_inverse_names() {
    for name in ["handle", "edge"] {
        let mut world = World::in_memory(schema().attribute(AttrDef::new(
            name,
            ValueType::Ref,
            Kind::State,
            Cardinality::Many,
        )))
        .unwrap_or_else(|error| panic!("{name} must be accepted at genesis: {error}"));
        let a = world.create_entity("CHARACTER", Some("a")).unwrap();
        let b = world.create_entity("CHARACTER", Some("b")).unwrap();
        world
            .assert(a, name, Value::Ref(b), Some(Tick(-10)))
            .unwrap();
        let edge = world.edge_id(a, name, b).unwrap();
        world
            .assert(
                edge.into(),
                "title",
                Value::Str("edge property".into()),
                Some(Tick(0)),
            )
            .unwrap();
        assert_eq!(
            world.parse_id(&world.format_id(edge.into())).unwrap(),
            EntityId::from(edge)
        );
        assert_eq!(world.as_of(a, name, Tick(0)).unwrap(), [Value::Ref(b)]);
        world
            .assert(edge.into(), name, Value::Ref(a), Some(Tick(0)))
            .unwrap();
        assert_eq!(
            world.as_of(edge.into(), name, Tick(0)).unwrap(),
            [Value::Ref(a)]
        );
        let mut inverse_world = World::in_memory(schema().attribute(
            AttrDef::new("relation", ValueType::Ref, Kind::State, Cardinality::Many).inverse(name),
        ))
        .unwrap();
        let a = inverse_world.create_entity("CHARACTER", None).unwrap();
        let b = inverse_world.create_entity("CHARACTER", None).unwrap();
        inverse_world
            .assert(a, "relation", Value::Ref(b), Some(Tick(0)))
            .unwrap();
        let neighbors = inverse_world
            .neighbors(b, name, Direction::Out, Tick(0))
            .unwrap();
        assert_eq!(
            neighbors
                .iter()
                .map(|(entity, _)| *entity)
                .collect::<Vec<_>>(),
            [a]
        );
        let mut world = World::in_memory(schema()).unwrap();
        let a = world.create_entity("CHARACTER", None).unwrap();
        let b = world.create_entity("CHARACTER", None).unwrap();
        let mut tx = world.begin();
        tx.infer_attribute(name, &Value::Ref(b)).unwrap();
        tx.assert(a, name, Value::Ref(b), Some(Tick(0))).unwrap();
        tx.commit().unwrap();
        assert_eq!(world.as_of(a, name, Tick(0)).unwrap(), [Value::Ref(b)]);
        let mut tx = world.begin();
        tx.declare_attribute(AttrDef::new(
            name,
            ValueType::Ref,
            Kind::State,
            Cardinality::Many,
        ))
        .unwrap();
        tx.commit().unwrap();
        let mut renamed = World::in_memory(schema()).unwrap();
        let mut tx = renamed.begin();
        tx.rename_attribute("title", name).unwrap();
        tx.commit().unwrap();
        assert!(renamed.attribute(name).is_some());
        assert!(open_appended(attribute(1, name, 4)).is_ok());
        assert!(open_appended(relation(1, "relation", name)).is_ok());
    }
}

#[test]
fn edge_node_type_is_refused_live_and_on_replay() {
    let mut world = World::in_memory(schema()).unwrap();
    assert!(matches!(
        world.begin().declare_node_type("edge"),
        Err(Error::InvalidName(_))
    ));
    assert!(matches!(
        world.begin().infer_node_type("edge"),
        Err(Error::InvalidName(_))
    ));
    assert!(matches!(
        World::in_memory(Schema::new().node_type("edge")),
        Err(Error::InvalidName(_))
    ));
    assert!(matches!(
        open_appended(node_type(1, "edge")),
        Err(Error::Corrupt(_))
    ));
}
