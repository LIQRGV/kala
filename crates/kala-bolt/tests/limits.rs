#[path = "common/client.rs"]
mod client;
mod common;
use client::{ALLIES, Client, map};
use common::{Running, assert_closed, handshake};
use kala_bolt::{Server, ServerConfig};
use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::{Duration, Instant};

#[test]
fn oversized_message_is_refused_and_connection_closed() {
    let server = Running::with_config(ServerConfig {
        max_message_bytes: 128,
        ..ServerConfig::default()
    });
    let mut client = Client::ready(&server);
    client.stream.write_all(&[0, 129]).unwrap();
    client.stream.write_all(&[0xc0; 129]).unwrap();
    client.stream.write_all(&[0, 0]).unwrap();
    client.failure("Neo.ClientError.Request.Invalid");
    assert_closed(&mut client.stream);
}

#[test]
fn stalled_handshake_and_idle_connection_time_out() {
    let server = Running::with_config(ServerConfig {
        handshake_timeout: Duration::from_millis(80),
        idle_timeout: Duration::from_millis(80),
        ..ServerConfig::default()
    });
    let mut handshake_only = server.connect();
    handshake_only.write_all(&[0x60]).unwrap();
    assert_closed(&mut handshake_only);
    let mut idle = Client::ready(&server);
    assert_closed(&mut idle.stream);
}

#[test]
fn idle_timeout_applies_to_partial_chunk_and_chunk_noops() {
    let server = Running::with_config(ServerConfig {
        idle_timeout: Duration::from_millis(80),
        ..ServerConfig::default()
    });
    let mut client = Client::ready(&server);
    client.stream.write_all(&[0, 10, 0xb1]).unwrap();
    assert_closed(&mut client.stream);
    let mut client = Client::ready(&server);
    client.stream.write_all(&[0, 0, 0, 0]).unwrap();
    assert_closed(&mut client.stream);
}

#[test]
fn connection_cap_rejects_excess_and_reclaims_slots() {
    let server = Running::with_config(ServerConfig {
        max_connections: 1,
        ..ServerConfig::default()
    });
    let mut first = Client::ready(&server);
    let mut excess = server.connect();
    assert_closed(&mut excess);
    assert_eq!(first.query(ALLIES).len(), 2);
    first.send(0x02, vec![]);
    assert_closed(&mut first.stream);
    // EOF is observable before the worker's RAII slot is released; retry the
    // handshake until that release, with a deadline only to diagnose hangs.
    wait_for_slot(&server);
}

fn wait_for_slot(server: &Running) {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        assert!(
            Instant::now() < deadline,
            "connection slot was not reclaimed"
        );
        let mut stream = server.connect();
        let proposal = [
            0x60, 0x60, 0xb0, 0x17, 0, 0, 4, 5, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        ];
        if stream.write_all(&proposal).is_err() {
            continue;
        }
        let mut response = [0; 4];
        if stream.read_exact(&mut response).is_ok() {
            assert_eq!(response, [0, 0, 4, 5]);
            return;
        }
    }
}

#[test]
fn no_auth_library_binding_refuses_non_loopback() {
    let dir = tempfile::tempdir().unwrap();
    let config = ServerConfig {
        listen: "0.0.0.0:0".parse().unwrap(),
        ..ServerConfig::default()
    };
    assert!(Server::bind(dir.path(), config).is_err());
}

#[test]
fn ipv6_loopback_can_negotiate_when_available() {
    let probe = match std::net::TcpListener::bind("[::1]:0") {
        Ok(probe) => probe,
        Err(error) => {
            eprintln!("skipping IPv6: loopback unavailable: {error}");
            return;
        }
    };
    drop(probe);
    let dir = tempfile::tempdir().unwrap();
    let server = Running::in_dir(
        dir,
        ServerConfig {
            listen: "[::1]:0".parse().unwrap(),
            ..ServerConfig::default()
        },
    );
    let mut client = TcpStream::connect(server.addr).unwrap();
    client
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    handshake(&mut client, 4);
}

#[test]
fn client_that_never_reads_releases_slot_after_write_timeout() {
    let dir = tempfile::tempdir().unwrap();
    let handles = (0..128)
        .map(|n| format!("e{n}"))
        .collect::<Vec<_>>()
        .join(", ");
    let mut yaml = format!(
        "schema:\n  node_types: [CHARACTER]\n  attributes:\n    - {{ name: biography, type: string, indexed: false }}\n    - {{ name: name, type: string }}\nentities:\n  CHARACTER: [{handles}]\ntransactions:\n  - tick: 0\n    ops:\n      - assert: [e0, biography, \"{}\"]\n",
        "x".repeat(100_000)
    );
    for n in 0..128 {
        yaml.push_str(&format!("      - assert: [e{n}, name, \"e{n}\"]\n"));
    }
    drop(kala_core::load_fixture_to(&yaml, &dir.path().join("large.kala")).unwrap());
    let server = Running::in_dir(
        dir,
        ServerConfig {
            listen: "127.0.0.1:0".parse().unwrap(),
            max_connections: 1,
            write_timeout: Duration::from_millis(80),
            ..ServerConfig::default()
        },
    );
    let mut blocked = Client::ready(&server);
    blocked.run_empty("// kala:datalog\n[:find ?h ?b :where [?e :name ?h] [:e0 :biography ?b]]");
    blocked.send(
        0x3f,
        vec![map(&[("n", kala_bolt::packstream::Value::Integer(-1))])],
    );
    // About 13 MiB exceeds the kernel send buffer. Keep this socket alive
    // without consuming records while checking another client can connect.
    wait_for_slot(&server);
    drop(blocked);
}
