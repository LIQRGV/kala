#[path = "common/client.rs"]
mod client;
mod common;
use client::{ALLIES, Client, NAMES, map, string, structure};
use common::{Running, assert_closed};
use kala_bolt::packstream::Value;

#[test]
fn handwritten_hello_and_run_then_pull_have_spec_metadata() {
    let server = Running::start();
    let mut client = Client::connected(&server, 4);
    client.hello();
    let hello = client.success();
    assert!(matches!(hello.get("server"), Some(Value::String(_))));
    assert!(matches!(hello.get("connection_id"), Some(Value::String(_))));
    assert_eq!(hello.get("hints"), Some(&map(&[])));
    client.send(0x6a, vec![map(&[("scheme", string("none"))])]);
    client.success();
    let mut run = vec![0xb3, 0x10, 0xd0, ALLIES.len() as u8];
    run.extend_from_slice(ALLIES.as_bytes());
    run.extend_from_slice(&[0xa0, 0xa0]);
    client.raw(&run);
    let success = client.success();
    assert_eq!(success.get("fields"), Some(&Value::List(vec![string("a")])));
    let (rows, meta) = client.pull(-1);
    assert_eq!(rows.len(), 2);
    assert_eq!(meta.get("type"), Some(&string("r")));
    assert_eq!(meta.get("db"), Some(&string("varrow")));
    assert!(matches!(meta.get("t_last"), Some(Value::Integer(n)) if *n >= 0));
}

#[test]
fn hello_auth_in_50_and_logon_in_51_are_accepted() {
    let server = Running::start();
    let mut old = Client::connected(&server, 0);
    old.send(
        0x01,
        vec![map(&[
            ("user_agent", string("test")),
            ("scheme", string("basic")),
            ("principal", string("test")),
            ("credentials", string("test-password")),
        ])],
    );
    old.success();
    assert_eq!(old.query(ALLIES).len(), 2);
    let mut new = Client::connected(&server, 1);
    new.hello();
    new.success();
    new.send(0x6a, vec![map(&[("scheme", string("none"))])]);
    new.success();
    assert_eq!(new.query(ALLIES).len(), 2);
    new.send(0x6b, vec![]);
    new.success();
    new.send(
        0x6a,
        vec![map(&[
            ("scheme", string("basic")),
            ("principal", string("test")),
            ("credentials", string("test-password")),
        ])],
    );
    new.success();
    assert_eq!(new.query(ALLIES).len(), 2);
}

#[test]
fn pull_partial_and_discard_preserve_one_result() {
    let server = Running::start();
    let mut client = Client::ready(&server);
    client.run_empty(NAMES);
    let (first, meta) = client.pull(5);
    assert_eq!(first.len(), 5);
    assert_eq!(meta.get("has_more"), Some(&Value::Bool(true)));
    client.send(0x2f, vec![map(&[("n", Value::Integer(5))])]);
    assert_eq!(client.success().get("has_more"), Some(&Value::Bool(true)));
    client.send(
        0x2f,
        vec![map(&[
            ("n", Value::Integer(-1)),
            ("qid", Value::Integer(-1)),
        ])],
    );
    assert_ne!(client.success().get("has_more"), Some(&Value::Bool(true)));
    assert_eq!(client.query(ALLIES).len(), 2);
}

#[test]
fn fetch_five_returns_all_rows_once() {
    let server = Running::start();
    let mut client = Client::ready(&server);
    let expected = client.query(NAMES);
    assert!(expected.len() >= 30);
    client.run_empty(NAMES);
    let mut actual = vec![];
    loop {
        let (rows, meta) = client.pull(5);
        assert!(rows.len() <= 5);
        actual.extend(rows);
        if meta.get("has_more") != Some(&Value::Bool(true)) {
            break;
        }
    }
    assert_eq!(actual, expected);
}

#[test]
fn invalid_state_is_failed_then_ignored_until_reset() {
    let server = Running::start();
    let mut client = Client::ready(&server);
    client.send(0x3f, vec![map(&[("n", Value::Integer(-1))])]);
    client.failure("Neo.ClientError.Request.Invalid");
    client.send(0x10, vec![string(ALLIES), map(&[]), map(&[])]);
    assert!(structure(client.recv(), 0x7e).is_empty());
    client.reset();
    assert_eq!(client.query(ALLIES).len(), 2);
}

#[test]
fn unsupported_transactions_and_routing_fail_cleanly() {
    let server = Running::start();
    let mut client = Client::ready(&server);
    for (tag, fields) in [
        (0x11, vec![map(&[])]),
        (0x12, vec![]),
        (0x13, vec![]),
        (0x66, vec![map(&[]), Value::List(vec![]), map(&[])]),
    ] {
        client.send(tag, fields);
        let failure = client.failure("Neo.ClientError.Request.Invalid");
        assert!(
            matches!(failure.get("message"), Some(Value::String(text)) if text.contains("not supported yet"))
        );
        client.reset();
        assert_eq!(client.query(ALLIES).len(), 2);
    }
}

#[test]
fn telemetry_acknowledged_goodbye_closes_and_unknown_tag_fails() {
    let server = Running::start();
    let mut client = Client::ready(&server);
    client.send(0x54, vec![Value::Integer(2)]);
    client.success();
    client.send(0x02, vec![]);
    assert_closed(&mut client.stream);
    let mut other = Client::ready(&server);
    other.raw(&[0xb0, 0x7a]);
    other.failure("Neo.ClientError.Request.Invalid");
}

#[test]
fn invalid_pull_counts_and_qid_are_refused() {
    let server = Running::start();
    let mut client = Client::ready(&server);
    for extra in [
        map(&[("n", Value::Integer(-2))]),
        map(&[("n", string("all"))]),
        map(&[("n", Value::Integer(1)), ("qid", Value::Integer(10))]),
    ] {
        client.run_empty(ALLIES);
        client.send(0x3f, vec![extra]);
        client.failure("Neo.ClientError.Request.Invalid");
        client.reset();
    }
}

#[test]
fn fragmented_chunks_and_chunk_noops_preserve_message_boundaries() {
    use std::io::Write;
    let server = Running::start();
    let mut client = Client::connected(&server, 4);
    // Empty chunks are keepalives between messages; two nonempty chunks
    // form one HELLO, with a split in the middle of a PackStream structure.
    client
        .stream
        .write_all(&[0, 0, 0, 1, 0xb1, 0, 2, 0x01, 0xa0, 0, 0])
        .unwrap();
    client.success();
    client.send(0x6a, vec![map(&[("scheme", string("none"))])]);
    client.success();
    assert_eq!(client.query(ALLIES).len(), 2);
}

#[test]
fn malformed_packstream_is_failure_and_terminal() {
    let server = Running::start();
    let mut client = Client::ready(&server);
    client.raw(&[0xd2, 0xff, 0xff, 0xff, 0xff]);
    client.failure("Neo.ClientError.Request.Invalid");
    assert_closed(&mut client.stream);
}
