//! PR #23 (slice 17) review polish, against the real server over a socket.
#[path = "common/client.rs"]
mod client;
mod common;
#[path = "common/tls.rs"]
mod tls;

use client::{Client, map, string};
use common::Running;
use kala_bolt::transport::{ListenAddr, Peer, ShutdownHandle, Transport};
use kala_bolt::{Server, ServerConfig};
use std::io::{self, Read, Write};
use std::sync::{Arc, Mutex};
use std::time::Instant;

/// Records the size of every write the server makes below its framing (and
/// below TLS, on a TLS listener).
struct Counting {
    inner: Box<dyn Transport>,
    writes: Arc<Mutex<Vec<usize>>>,
}
impl Transport for Counting {
    fn read_until(&mut self, bytes: &mut [u8], deadline: Instant) -> io::Result<usize> {
        self.inner.read_until(bytes, deadline)
    }
    fn write_until(&mut self, bytes: &[u8], deadline: Instant) -> io::Result<usize> {
        let n = self.inner.write_until(bytes, deadline)?;
        self.writes.lock().unwrap().push(n);
        Ok(n)
    }
    fn finish(&mut self, deadline: Instant) -> io::Result<()> {
        self.inner.finish(deadline)
    }
    fn shutdown_handle(&self) -> io::Result<Box<dyn ShutdownHandle>> {
        self.inner.shutdown_handle()
    }
    fn peer(&self) -> io::Result<Peer> {
        self.inner.peer()
    }
    fn local_addr(&self) -> io::Result<ListenAddr> {
        self.inner.local_addr()
    }
}

fn counting_server(listen: &str) -> (Running, Arc<Mutex<Vec<usize>>>) {
    let dir = tempfile::tempdir().unwrap();
    let mut config = ServerConfig {
        listen: vec![listen.parse().unwrap()],
        ..ServerConfig::default()
    };
    if listen.starts_with("bolt+s") {
        tls::configure(dir.path(), &mut config);
    }
    let writes = Arc::new(Mutex::new(vec![]));
    let record = writes.clone();
    let server = Server::bind_accounts(dir.path(), config)
        .unwrap()
        .with_transport_wrapper(move |inner| {
            Box::new(Counting {
                inner,
                writes: record.clone(),
            })
        });
    (Running::from_server(dir, server), writes)
}

fn logged_on<S: Read + Write>(stream: S) -> Client<S> {
    let mut c = Client { stream };
    common::handshake(&mut c.stream, 4);
    c.hello();
    c.success();
    c.send(0x6a, vec![map(&[("scheme", string("none"))])]);
    c.success();
    c
}

// A Bolt message used to cost three writes (chunk header, body, terminator):
// three syscalls on TCP and three TLS records, which also showed chunk
// boundaries on the wire. It is one write on every transport.
#[test]
fn one_small_response_is_one_write_below_tls_and_on_plain_tcp() {
    // SUCCESS {} framed: 2 + 3 + 2 bytes; a TLS 1.3 record adds 5 bytes of
    // header, 1 of content type and a 16-byte tag.
    for (listen, size) in [("bolt+s://127.0.0.1:0", 7 + 22), ("127.0.0.1:0", 7)] {
        let (server, writes) = counting_server(listen);
        let after_logon = |writes: &Mutex<Vec<usize>>| writes.lock().unwrap().len();
        if listen.starts_with("bolt+s") {
            let mut c = logged_on(tls::stream(server.dir.path(), server.connect()));
            let before = after_logon(&writes);
            c.send(0x0f, vec![]);
            c.success();
            assert_eq!(writes.lock().unwrap()[before..], [size], "{listen}");
        } else {
            let mut c = logged_on(server.connect());
            let before = after_logon(&writes);
            c.send(0x0f, vec![]);
            c.success();
            assert_eq!(writes.lock().unwrap()[before..], [size], "{listen}");
        }
    }
}

/// Completes a TLS handshake and returns the leaf the server presented.
fn presented_leaf(server: &Running) -> Vec<u8> {
    let mut stream = tls::stream(server.dir.path(), server.connect());
    while stream.conn.is_handshaking() {
        stream.conn.complete_io(&mut stream.sock).unwrap();
    }
    stream.conn.peer_certificates().unwrap()[0].to_vec()
}

fn leaf_on_disk(dir: &std::path::Path) -> Vec<u8> {
    use rustls::pki_types::{CertificateDer, pem::PemObject};
    CertificateDer::from_pem_file(dir.join("test-cert.pem"))
        .unwrap()
        .to_vec()
}

// `TlsReloader::reload` had no `cargo test` coverage (only the opt-in driver
// E2E reached it, through a signal). Called directly: a valid new pair is
// what new connections get, a connection admitted before keeps working, and
// an invalid pair returns an error and changes nothing.
#[test]
fn reload_installs_a_valid_pair_and_keeps_the_old_one_on_any_error() {
    let dir = tempfile::tempdir().unwrap();
    let mut config = ServerConfig {
        listen: vec!["bolt+s://127.0.0.1:0".parse().unwrap()],
        ..ServerConfig::default()
    };
    tls::configure(dir.path(), &mut config);
    let (cert, key) = (
        config.tls_cert.clone().unwrap(),
        config.tls_key.clone().unwrap(),
    );
    let server = Server::bind_accounts(dir.path(), config).unwrap();
    let reloader = server
        .tls_reloader()
        .expect("a TLS listener has a reloader");
    let server = Running::from_server(dir, server);
    let first = leaf_on_disk(server.dir.path());
    assert_eq!(presented_leaf(&server), first);
    let mut pinned = logged_on(tls::stream(server.dir.path(), server.connect()));

    // A new valid pair.
    let install = || {
        let pair = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
        std::fs::write(&cert, pair.cert.pem()).unwrap();
        std::fs::write(&key, pair.signing_key.serialize_pem()).unwrap();
        pair.cert.der().to_vec()
    };
    let second = install();
    assert_ne!(first, second);
    // Nothing changes until reload is called.
    reloader.reload().unwrap();
    assert_eq!(presented_leaf(&server), second);
    pinned.send(0x0f, vec![]);
    pinned.success();

    // Invalid pairs: each is refused and the second identity stays.
    let good_cert = std::fs::read(&cert).unwrap();
    let good_key = std::fs::read(&key).unwrap();
    let marker = "PRIVATE-TEST-MARKER-DO-NOT-ECHO";
    let other = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
    let cases: [(&str, &std::path::Path, Vec<u8>, &str); 4] = [
        ("garbage key", &key, marker.into(), "test-key.pem"),
        (
            "key of another certificate",
            &key,
            other.signing_key.serialize_pem().into(),
            "does not match",
        ),
        ("empty chain", &cert, vec![], "test-cert.pem"),
        (
            "certificate of another key",
            &cert,
            other.cert.pem().into(),
            "does not match",
        ),
    ];
    for (name, path, contents, expected) in cases {
        std::fs::write(path, contents).unwrap();
        let error = reloader.reload().expect_err(name).to_string();
        assert!(error.contains(expected), "{name}: {error}");
        assert!(!error.contains(marker), "{name}: key contents in the error");
        std::fs::write(&cert, &good_cert).unwrap();
        std::fs::write(&key, &good_key).unwrap();
        // The client trusts the certificate on disk, which is `second` again.
        assert_eq!(presented_leaf(&server), second, "{name}");
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o640)).unwrap();
        let error = reloader.reload().unwrap_err().to_string();
        assert!(error.contains("mode 0640"), "{error}");
        std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(presented_leaf(&server), second);
    }
    // After the failures a good pair is still accepted.
    let third = install();
    reloader.reload().unwrap();
    assert_eq!(presented_leaf(&server), third);
    pinned.send(0x0f, vec![]);
    pinned.success();
}

// A server without a TLS listener has nothing to reload.
#[test]
fn a_server_without_a_tls_listener_has_no_reloader() {
    let dir = tempfile::tempdir().unwrap();
    let config = ServerConfig {
        listen: vec!["127.0.0.1:0".parse().unwrap()],
        ..ServerConfig::default()
    };
    assert!(
        Server::bind(dir.path(), config)
            .unwrap()
            .tls_reloader()
            .is_none()
    );
}
