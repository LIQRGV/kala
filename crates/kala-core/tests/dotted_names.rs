//! User identifiers are flat in both live transactions and logs from earlier binaries.
mod common;

use kala_core::*;
use std::fs;

fn scalar(name: &str) -> AttrDef {
    AttrDef::new(name, ValueType::Str, Kind::State, Cardinality::One)
}

fn relation(inverse: &str) -> AttrDef {
    AttrDef::new("owns", ValueType::Ref, Kind::State, Cardinality::Many).inverse(inverse)
}

fn refused(world: &mut World, name: &str, write: impl FnOnce(&mut Txn<'_>) -> Result<(), Error>) {
    let before = world.latest_tx();
    let mut tx = world.begin();
    tx.create_entity("NPC", Some("staged")).unwrap();
    let message = write(&mut tx)
        .expect_err("accepted a dotted name")
        .to_string();
    assert!(
        message.contains(name) && message.contains("may not contain '.'"),
        "{message}"
    );
    assert_eq!(tx.commit(), Err(Error::TransactionAborted));
    assert_eq!(world.latest_tx(), before);
    assert!(world.entity_by_handle("staged").is_none());
}

#[test]
fn dotted_names_are_refused_before_commit() {
    let mut world = World::in_memory(Schema::new().node_type("NPC")).unwrap();
    refused(&mut world, "a.b", |tx| {
        tx.declare_attribute(scalar("a.b")).map(|_| ())
    });
    refused(&mut world, "owned.by", |tx| {
        tx.declare_attribute(relation("owned.by")).map(|_| ())
    });
    refused(&mut world, "A.B", |tx| {
        tx.declare_node_type("A.B").map(|_| ())
    });
    refused(&mut world, "a.b", |tx| {
        tx.create_entity("NPC", Some("a.b")).map(|_| ())
    });
}

#[test]
fn dotted_names_in_older_logs_report_the_name_and_recovery_steps() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("world.kala");
    let schema = Schema::new()
        .node_type("Type_genesis")
        .attribute(scalar("attr_genesis"))
        .attribute(relation("inverse_genesis"));
    let mut world = World::create(&path, schema).unwrap();
    let mut tx = world.begin();
    tx.declare_node_type("Type_edit").unwrap();
    tx.declare_attribute(scalar("attr_edit")).unwrap();
    tx.create_entity("Type_edit", Some("handle_edit")).unwrap();
    tx.commit().unwrap();
    drop(world);
    drop(World::open_read_only(&path).unwrap());

    let valid = fs::read(&path).unwrap();
    for name in [
        "Type_genesis",
        "attr_genesis",
        "inverse_genesis",
        "Type_edit",
        "attr_edit",
        "handle_edit",
    ] {
        let mut bytes = valid.clone();
        let start = bytes
            .windows(name.len())
            .position(|window| window == name.as_bytes())
            .unwrap();
        let dotted = name.replace('_', ".");
        bytes[start..start + name.len()].copy_from_slice(dotted.as_bytes());
        common::rechain(&mut bytes, 0);
        fs::write(&path, &bytes).unwrap();
        let Err(error) = World::open_read_only(&path) else {
            panic!("older log with {dotted} opened")
        };
        assert!(matches!(error, Error::Corrupt(_)), "{error}");
        let text = error.to_string();
        for expected in [dotted.as_str(), "dump", "older", "rename", "reload"] {
            assert!(text.contains(expected), "missing {expected} in {text}");
        }
        assert_eq!(fs::read(&path).unwrap(), bytes);
    }
}
