//! Names starting with `_KALA_` are reserved for node types and attributes in
//! every schema edit after genesis. The system world's genesis may use them.

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
    body.extend_from_slice(&commit_frame(&body, id + 1, 1));
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
