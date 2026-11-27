//! Bolt over a TLS listener, starting with the TLS handshake on the wire.
#[path = "common/client.rs"]
mod client;
mod common;
#[path = "common/tls.rs"]
mod tls;

use client::{Client, map, string};
use common::Running;
use kala_bolt::transport::{ListenAddr, Peer};
use kala_bolt::{Server, ServerConfig};
use std::io::{self, Read, Write};
use std::net::{Ipv4Addr, TcpStream, UdpSocket};
use std::time::{Duration, Instant};

const LISTEN: &str = "bolt+s://127.0.0.1:0";
const BOLT: [u8; 20] = [
    0x60, 0x60, 0xb0, 0x17, 0, 0, 4, 5, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
];

fn server_at(listen: &str, mut config: ServerConfig) -> Running {
    let addr = listen.parse::<ListenAddr>();
    assert!(addr.is_ok(), "TLS listener must parse: {:?}", addr.err());
    let dir = tempfile::tempdir().unwrap();
    config.listen = vec![addr.unwrap()];
    tls::configure(dir.path(), &mut config);
    let server = Server::bind_accounts(dir.path(), config).unwrap();
    Running::from_server(dir, server)
}

fn client_hello(server: &Running) -> Vec<u8> {
    let mut hello = vec![];
    tls::client(server.dir.path())
        .write_tls(&mut hello)
        .unwrap();
    hello
}

fn logon(
    stream: rustls::StreamOwned<rustls::ClientConnection, TcpStream>,
) -> Client<rustls::StreamOwned<rustls::ClientConnection, TcpStream>> {
    let mut c = Client { stream };
    common::handshake(&mut c.stream, 4);
    c.hello();
    c.success();
    c.send(0x6a, vec![map(&[("scheme", string("none"))])]);
    c
}

#[test]
fn bolt_handshake_and_hello_over_tls() {
    let server = server_at(LISTEN, ServerConfig::default());
    let mut c = logon(tls::stream(server.dir.path(), server.connect()));
    c.success();
    assert!(c.stream.conn.alpn_protocol().is_none());
    assert_eq!(
        c.stream.conn.protocol_version(),
        Some(rustls::ProtocolVersion::TLSv1_3)
    );
}

#[test]
fn clients_can_negotiate_tls_1_2() {
    let server = server_at(LISTEN, ServerConfig::default());
    let conn = tls::client_with_versions(server.dir.path(), &[&rustls::version::TLS12]);
    let mut c = logon(rustls::StreamOwned::new(conn, server.connect()));
    c.success();
    assert_eq!(
        c.stream.conn.protocol_version(),
        Some(rustls::ProtocolVersion::TLSv1_2)
    );
}

#[test]
fn goodbye_sends_tls_close_notify() {
    let server = server_at(LISTEN, ServerConfig::default());
    let mut c = logon(tls::stream(server.dir.path(), server.connect()));
    c.success();
    c.send(0x02, vec![]);
    // rustls reports UnexpectedEof for a TCP close without close_notify.
    assert_eq!(c.stream.read(&mut [0]).unwrap(), 0);
}

#[test]
fn peer_close_notify_receives_a_clean_tls_eof() {
    let server = server_at(LISTEN, ServerConfig::default());
    let mut c = logon(tls::stream(server.dir.path(), server.connect()));
    c.success();
    c.stream.conn.send_close_notify();
    c.stream.flush().unwrap();
    let reply = c.stream.read(&mut [0]);
    assert!(
        matches!(reply, Ok(0)),
        "expected reciprocal close_notify, got {reply:?}"
    );
}

// A TLS failure may send a fatal alert before closing the underlying socket.
fn assert_tls_refused(stream: TcpStream) {
    let mut reply = vec![];
    match stream.take(8).read_to_end(&mut reply) {
        Ok(_) => {}
        Err(e)
            if matches!(
                e.kind(),
                io::ErrorKind::ConnectionReset | io::ErrorKind::ConnectionAborted
            ) => {}
        Err(e) => panic!("expected TLS refusal, got {e}"),
    }
    if !reply.is_empty() {
        assert!(
            reply.len() == 7 && reply[0] == 21 && reply[5] == 2,
            "expected a fatal TLS alert, got {reply:?}"
        );
    }
}

#[test]
fn silent_and_partial_tls_handshakes_time_out() {
    let server = server_at(
        LISTEN,
        ServerConfig {
            handshake_timeout: Duration::from_millis(120),
            ..ServerConfig::default()
        },
    );
    let hello = client_hello(&server);
    for prefix in [&[][..], &hello[..hello.len() / 2]] {
        let start = Instant::now();
        let mut stream = server.connect();
        stream.write_all(prefix).unwrap();
        assert_tls_refused(stream);
        let elapsed = start.elapsed();
        assert!(
            (Duration::from_millis(100)..Duration::from_secs(2)).contains(&elapsed),
            "an incomplete handshake should remain open until its deadline, elapsed {elapsed:?}"
        );
    }
    logon(tls::stream(server.dir.path(), server.connect())).success();
}

#[test]
fn partial_encrypted_record_obeys_bolt_idle_timeout() {
    let server = server_at(
        LISTEN,
        ServerConfig {
            idle_timeout: Duration::from_millis(200),
            ..ServerConfig::default()
        },
    );
    let mut c = logon(tls::stream(server.dir.path(), server.connect()));
    c.success();
    let (mut conn, mut socket) = c.stream.into_parts();
    conn.writer().write_all(&[0, 2, 0xb0, 0x0f, 0, 0]).unwrap();
    let mut encrypted = vec![];
    conn.write_tls(&mut encrypted).unwrap();
    socket.write_all(&encrypted[..encrypted.len() - 1]).unwrap();
    let start = Instant::now();
    // Any bytes received here are encrypted (e.g. a close_notify); EOF or
    // reset must still arrive even though the client's last record is incomplete.
    let result = socket.take(1024).read_to_end(&mut vec![]);
    assert!(
        matches!(result, Ok(n) if n < 1024)
            || matches!(result, Err(ref e) if e.kind() == io::ErrorKind::ConnectionReset),
        "{result:?}"
    );
    assert!(start.elapsed() < Duration::from_secs(2));
}

#[test]
fn neither_listener_falls_back_to_the_other_protocol() {
    let tls = server_at(LISTEN, ServerConfig::default());
    let plain = Running::start();
    let mut socket = tls.connect();
    socket.write_all(&BOLT).unwrap();
    assert_tls_refused(socket);
    let mut socket = plain.connect();
    socket.write_all(&client_hello(&tls)).unwrap();
    common::assert_closed(&mut socket);
    logon(tls::stream(tls.dir.path(), tls.connect())).success();
    common::handshake(&mut plain.connect(), 4);
}

#[test]
fn tls_1_1_client_hello_is_refused() {
    let server = server_at(LISTEN, ServerConfig::default());
    // RFC 4346 ClientHello: highest version TLS 1.1, 32 random bytes, empty
    // session ID, TLS_ECDHE_ECDSA_WITH_AES_128_CBC_SHA, null compression.
    let mut hello = vec![22, 3, 2, 0, 45, 1, 0, 0, 41, 3, 2];
    hello.extend_from_slice(&[0x35; 32]);
    hello.extend_from_slice(&[0, 0, 2, 0xc0, 0x09, 1, 0]);
    let mut socket = server.connect();
    socket.write_all(&hello).unwrap();
    assert_tls_refused(socket);
}

#[test]
fn tls_keeps_non_loopback_peers_out_of_trust_accounts() {
    // connect() on UDP only asks for the local route. No packet is sent.
    let ip = UdpSocket::bind("0.0.0.0:0")
        .ok()
        .and_then(|socket| {
            socket.connect("192.0.2.1:9").ok()?;
            Some(socket.local_addr().ok()?.ip())
        })
        .filter(|ip| !ip.is_loopback() && !ip.is_unspecified());
    if let Some(ip) = ip {
        let mut server = server_at("bolt+s://0.0.0.0:0", ServerConfig::default());
        // Same listener, certificate, account and token. Only the peer that the
        // inner TCP transport reports changes between these connections.
        server.addr.set_ip(Ipv4Addr::LOCALHOST.into());
        logon(tls::stream(server.dir.path(), server.connect())).success();
        server.addr.set_ip(ip);
        let socket = server.connect();
        assert_eq!(socket.local_addr().unwrap().ip(), ip);
        logon(tls::stream(server.dir.path(), socket))
            .failure("Neo.ClientError.Security.Unauthorized");
    } else {
        // An isolated CI namespace may only have loopback. Check the same
        // account decision with the identity a TCP transport supplies.
        use kala_bolt::auth::{Accounts, AuthToken, Authenticator, Credentials};
        let dir = tempfile::tempdir().unwrap();
        let auth = Accounts::open(dir.path()).unwrap();
        let token = AuthToken {
            scheme: "none".into(),
            principal: String::new(),
            credentials: Credentials::new(String::new()),
        };
        let peer = "192.0.2.20".parse().unwrap();
        assert!(auth.authenticate_peer(&token, Peer::Ip(peer)).is_err());
        assert!(
            auth.authenticate_peer(&token, Peer::Ip("127.0.0.1".parse().unwrap()))
                .is_ok()
        );
    }
}

/// Reads what the server sends before it closes: at most `limit` bytes.
fn reply_before_close(stream: TcpStream, limit: u64) -> Vec<u8> {
    let mut reply = vec![];
    match stream.take(limit).read_to_end(&mut reply) {
        Ok(_) => {}
        Err(e) if e.kind() == io::ErrorKind::ConnectionReset => {}
        Err(e) => panic!("expected a close, got {e}"),
    }
    reply
}

// PR #23 polish: rustls queues a fatal alert when it rejects a handshake.
// The wrapper must flush it before closing, so the client learns why
// (protocol_version, handshake_failure) instead of seeing a bare close.
#[test]
fn a_rejected_handshake_sends_its_fatal_alert_before_the_close() {
    let server = server_at(LISTEN, ServerConfig::default());
    // Highest version TLS 1.1 (see `tls_1_1_client_hello_is_refused`).
    let mut tls11 = vec![22, 3, 2, 0, 45, 1, 0, 0, 41, 3, 2];
    tls11.extend_from_slice(&[0x35; 32]);
    tls11.extend_from_slice(&[0, 0, 2, 0xc0, 0x09, 1, 0]);
    // TLS 1.2 offering only TLS_RSA_WITH_RC4_128_MD5: no common cipher suite.
    let mut no_cipher = vec![22, 3, 3, 0, 45, 1, 0, 0, 41, 3, 3];
    no_cipher.extend_from_slice(&[0x35; 32]);
    no_cipher.extend_from_slice(&[0, 0, 2, 0x00, 0x04, 1, 0]);
    for (hello, name) in [(tls11, "TLS 1.1"), (no_cipher, "no common cipher")] {
        let start = Instant::now();
        let mut socket = server.connect();
        socket.write_all(&hello).unwrap();
        let reply = reply_before_close(socket, 64);
        // One plaintext alert record: type 21, version, length 2, level
        // fatal (2), then the description.
        assert!(
            reply.len() == 7 && reply[0] == 21 && reply[3..6] == [0, 2, 2],
            "{name}: expected one fatal alert record, got {reply:?}"
        );
        assert!(
            matches!(reply[6], 40 | 70 | 71),
            "{name}: alert description {}",
            reply[6]
        );
        assert!(start.elapsed() < Duration::from_secs(3), "{name}");
    }
    // The server keeps serving.
    logon(tls::stream(server.dir.path(), server.connect())).success();
}
