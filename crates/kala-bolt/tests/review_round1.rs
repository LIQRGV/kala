//! PR #4 review round 1: regressions for the polish findings.
#[path = "common/client.rs"]
mod client;
mod common;
use client::{ALLIES, Client, map, string};
use common::Running;
use kala_bolt::{ServerConfig, packstream::Value};
use kala_core::{AttrDef, Cardinality, Kind, Schema, Tick, Value as KValue, ValueType, World};
use std::path::Path;

fn hello_metadata(server: &Running) -> std::collections::BTreeMap<String, Value> {
    let mut c = Client::connected(server, 4);
    c.hello();
    c.success()
}

// Drivers must retire idle pooled connections before the server's idle
// timeout closes them: the HELLO hints say when.
#[test]
fn hello_hints_announce_the_idle_timeout() {
    let config = ServerConfig {
        idle_timeout: std::time::Duration::from_secs(123),
        ..ServerConfig::default()
    };
    let server = Running::with_config(config);
    let meta = hello_metadata(&server);
    let Value::Map(hints) = &meta["hints"] else {
        panic!("hints must be a map")
    };
    assert_eq!(
        hints.get("connection.recv_timeout_seconds"),
        Some(&Value::Integer(123))
    );
}

// The agent must start with `Neo4j/` (drivers check it) but must not claim to
// be Neo4j version 0.x: it advertises the Bolt-5 compatibility level and says
// it is Kala.
#[test]
fn server_agent_is_neo4j_5_compatible_and_names_kala() {
    let server = Running::start();
    let meta = hello_metadata(&server);
    let Value::String(agent) = &meta["server"] else {
        panic!("server must be a string")
    };
    assert_eq!(agent, "Neo4j/5.0.0");
    assert_eq!(
        meta.get("kala_version"),
        Some(&string(env!("CARGO_PKG_VERSION")))
    );
}

#[test]
fn crlf_prefix_is_datalog() {
    let server = Running::start();
    let mut c = Client::ready(&server);
    let rows = c.query(&ALLIES.replacen('\n', "\r\n", 1));
    assert_eq!(rows.len(), 2);
}

// Error positions refer to the text the client sent, prefix line included.
#[test]
fn error_lines_count_the_prefix_line() {
    let server = Running::start();
    let mut c = Client::ready(&server);
    c.send(
        0x10,
        vec![
            string("// kala:datalog\n[:find ?a\n :where [:edwin :bogus ?a]]"),
            map(&[]),
            map(&[]),
        ],
    );
    let meta = c.failure("Neo.ClientError.Statement.SyntaxError");
    let Value::String(message) = &meta["message"] else {
        panic!()
    };
    assert!(message.contains("line 3"), "{message}");
}

// A PackStream Bytes parameter is a type error, not a broken connection.
#[test]
fn bytes_parameter_is_a_type_error_and_the_connection_survives() {
    let server = Running::start();
    let mut c = Client::ready(&server);
    let q = "// kala:datalog\n[:find ?a :in $ ?who :where [?e :handle ?who] [?e :ally_of ?a]]";
    let mut msg = vec![0xb3, 0x10, 0xd0, q.len() as u8];
    msg.extend_from_slice(q.as_bytes());
    // {"who": Bytes[0x78]} and an empty extra map.
    msg.extend_from_slice(&[0xa1, 0x83, b'w', b'h', b'o', 0xcc, 0x01, 0x78, 0xa0]);
    c.raw(&msg);
    c.failure("Neo.ClientError.Statement.TypeError");
    c.reset();
    assert_eq!(c.query(ALLIES).len(), 2);
}

fn small_world(path: &Path, last: &str) {
    let schema = Schema::new().node_type("THING").attribute(AttrDef::new(
        "name",
        ValueType::Str,
        Kind::State,
        Cardinality::One,
    ));
    let mut w = World::create(path, schema).unwrap();
    let e = w.create_entity("THING", Some("t")).unwrap();
    w.assert(e, "name", KValue::Str(last.into()), Some(Tick(0)))
        .unwrap();
}

// A log replaced by different committed content of the same byte length
// (recovery can shrink a log and a new commit can grow it back) must reload.
#[test]
fn same_length_different_content_reloads() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("w.kala");
    small_world(&path, "aaaa");
    let other = tempfile::tempdir().unwrap();
    small_world(&other.path().join("w.kala"), "bbbb");
    let replacement = std::fs::read(other.path().join("w.kala")).unwrap();
    assert_eq!(
        replacement.len() as u64,
        std::fs::metadata(&path).unwrap().len()
    );
    let config = ServerConfig {
        listen: "127.0.0.1:0".parse().unwrap(),
        ..ServerConfig::default()
    };
    let server = Running::in_dir(dir, config);
    let mut c = Client::ready(&server);
    let q = "// kala:datalog\n[:find ?n :where [:t :name ?n]]";
    assert_eq!(c.query(q), [vec![string("aaaa")]]);
    std::fs::write(server.dir.path().join("w.kala"), replacement).unwrap();
    assert_eq!(c.query(q), [vec![string("bbbb")]]);
}

// `handle` is reserved for entity identity in the query and Bolt adapters.
// Rejecting the declaration prevents a property from shadowing that identity.
#[test]
fn reserved_handle_attribute_is_refused_before_creating_the_world_file() {
    let dir = tempfile::tempdir().unwrap();
    let schema = Schema::new().node_type("THING").attribute(AttrDef::new(
        "handle",
        ValueType::Str,
        Kind::State,
        Cardinality::One,
    ));
    assert!(matches!(
        World::in_memory(schema.clone()),
        Err(kala_core::Error::InvalidName(name)) if name == "handle"
    ));
    let path = dir.path().join("w.kala");
    assert!(matches!(
        World::create(&path, schema),
        Err(kala_core::Error::InvalidName(name)) if name == "handle"
    ));
    assert!(!path.exists(), "a refused schema must not create a log");
}

// Review round 2: a sub-second idle timeout sends no recv-timeout hint (0
// means "none" to some drivers).
#[test]
fn sub_second_idle_timeout_sends_no_hint() {
    let config = ServerConfig {
        idle_timeout: std::time::Duration::from_millis(500),
        ..ServerConfig::default()
    };
    let server = Running::with_config(config);
    let meta = hello_metadata(&server);
    assert_eq!(meta.get("hints"), Some(&map(&[])));
}
