//! PR #17 (slice 16) review polish, through the real `kala serve` binary:
//! SIGTERM runs the clean shutdown, and the auth log names the transport.
#![cfg(unix)]

#[path = "common/serving.rs"]
mod serving;

use serving::{Serving, short_dir};
use std::io::{Read, Write};
use std::net::TcpStream;
use std::os::unix::net::UnixStream;
use std::time::Duration;

/// One chunked Bolt message.
fn send(stream: &mut impl Write, message: &[u8]) {
    stream
        .write_all(&(message.len() as u16).to_be_bytes())
        .unwrap();
    stream.write_all(message).unwrap();
    stream.write_all(&[0, 0]).unwrap();
}

/// Reads one chunked message and requires it to be SUCCESS.
fn success(stream: &mut impl Read) {
    let mut message = Vec::new();
    loop {
        let mut header = [0; 2];
        stream.read_exact(&mut header).unwrap();
        let len = usize::from(u16::from_be_bytes(header));
        if len == 0 {
            break;
        }
        let start = message.len();
        message.resize(start + len, 0);
        stream.read_exact(&mut message[start..]).unwrap();
    }
    assert_eq!(message[..2], [0xb1, 0x70], "expected SUCCESS: {message:?}");
}

/// Handshake (Bolt 5.4), HELLO and LOGON with scheme "none".
fn log_on(stream: &mut (impl Read + Write)) {
    stream
        .write_all(&[
            0x60, 0x60, 0xb0, 0x17, 0, 0, 4, 5, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        ])
        .unwrap();
    let mut version = [0; 4];
    stream.read_exact(&mut version).unwrap();
    assert_eq!(version, [0, 0, 4, 5]);
    // HELLO {user_agent: "test"}
    send(
        stream,
        &[
            0xb1, 0x01, 0xa1, 0x8a, b'u', b's', b'e', b'r', b'_', b'a', b'g', b'e', b'n', b't',
            0x84, b't', b'e', b's', b't',
        ],
    );
    success(stream);
    // LOGON {scheme: "none"}
    send(
        stream,
        &[
            0xb1, 0x6a, 0xa1, 0x86, b's', b'c', b'h', b'e', b'm', b'e', 0x84, b'n', b'o', b'n',
            b'e',
        ],
    );
    success(stream);
}

// SIGHUP leaves a Unix-only listener running. SIGTERM drains its active
// sessions and removes the socket published at startup.
#[test]
fn sighup_preserves_the_unix_listener_and_sigterm_shuts_down_cleanly() {
    let dir = short_dir();
    let path = dir.path().join("bolt.sock");
    let unix = format!("unix://{}", path.display());
    let mut serving = Serving::start(dir.path(), &[&unix], &["--auth", "none"]);
    serving.send_signal("-HUP");
    let mut client = UnixStream::connect(&path).unwrap();
    client
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    log_on(&mut client);
    let status = serving.signal("-TERM");
    assert!(
        status.success(),
        "SIGTERM returned {status:?}; {}",
        serving.stderr()
    );
    assert!(!path.exists(), "socket file survived SIGTERM");
    let mut byte = [0];
    assert_eq!(client.read(&mut byte).unwrap(), 0);
    assert!(serving.stderr().contains("event=bolt_disconnected"));
}

// A Unix peer has no IP: the auth log says so instead of `localhost`, and
// names the transport for both kinds of peer.
#[test]
fn the_auth_log_names_the_transport_and_gives_unix_peers_no_ip() {
    let dir = short_dir();
    let path = dir.path().join("bolt.sock");
    let unix = format!("unix://{}", path.display());
    let mut serving = Serving::start(dir.path(), &[&unix, "127.0.0.1:0"], &["--auth", "none"]);
    let mut local = UnixStream::connect(&path).unwrap();
    local
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    log_on(&mut local);
    let mut tcp = TcpStream::connect(&serving.addresses[1]).unwrap();
    tcp.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    log_on(&mut tcp);
    assert!(serving.signal("-INT").success());
    let stderr = serving.stderr();
    assert!(
        stderr.contains("event=bolt_authenticated connection_id=1 user=\"\" client_ip=- peer=unix"),
        "{stderr}"
    );
    assert!(
        stderr.contains(
            "event=bolt_authenticated connection_id=2 user=\"\" client_ip=127.0.0.1 peer=tcp"
        ),
        "{stderr}"
    );
    assert!(!stderr.contains("client_ip=localhost"), "{stderr}");
}
