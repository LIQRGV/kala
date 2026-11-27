//! PR #23 (slice 17) review polish, through the real `kala serve` binary:
//! SIGHUP stops the server again, SIGUSR1 reloads the TLS identity, and a
//! log write that fails never takes a thread (or the server) down.
#![cfg(unix)]

#[path = "common/bolt.rs"]
mod bolt;
#[path = "common/serving.rs"]
mod serving;

use bolt::{log_on, send, success};
use rcgen::{CertificateParams, KeyPair};
use rustls::pki_types::{CertificateDer, ServerName};
use rustls::{ClientConfig, ClientConnection, RootCertStore, StreamOwned};
use serving::{Serving, short_dir};
use std::fs;
use std::io::Read;
use std::net::TcpStream;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

/// Writes a fresh self-signed pair and returns the leaf's DER.
fn install_pair(dir: &Path) -> CertificateDer<'static> {
    let key = KeyPair::generate().unwrap();
    let cert = CertificateParams::new(vec!["localhost".into()])
        .unwrap()
        .self_signed(&key)
        .unwrap();
    // Replace by rename, as a careful rotation does.
    for (name, pem) in [("chain.pem", cert.pem()), ("key.pem", key.serialize_pem())] {
        let staged = dir.join(format!("{name}.new"));
        fs::write(&staged, pem).unwrap();
        fs::set_permissions(&staged, fs::Permissions::from_mode(0o600)).unwrap();
        fs::rename(staged, dir.join(name)).unwrap();
    }
    cert.der().clone()
}

fn tls_flags(dir: &Path) -> [String; 4] {
    [
        "--tls.cert".into(),
        dir.join("chain.pem").to_str().unwrap().into(),
        "--tls.key".into(),
        dir.join("key.pem").to_str().unwrap().into(),
    ]
}

type TlsStream = StreamOwned<ClientConnection, TcpStream>;

/// A TLS connection that trusts exactly `roots`, after its handshake.
fn connect(address: &str, roots: &[&CertificateDer<'static>]) -> TlsStream {
    let mut store = RootCertStore::empty();
    for root in roots {
        store.add((*root).clone()).unwrap();
    }
    let config = ClientConfig::builder()
        .with_root_certificates(store)
        .with_no_client_auth();
    let name = ServerName::try_from("localhost").unwrap();
    let conn = ClientConnection::new(Arc::new(config), name).unwrap();
    let socket = TcpStream::connect(address.strip_prefix("bolt+s://").unwrap()).unwrap();
    socket
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let mut stream = StreamOwned::new(conn, socket);
    while stream.conn.is_handshaking() {
        stream.conn.complete_io(&mut stream.sock).unwrap();
    }
    stream
}

/// The leaf a new connection is given.
fn leaf(address: &str, roots: &[&CertificateDer<'static>]) -> CertificateDer<'static> {
    let stream = connect(address, roots);
    stream.conn.peer_certificates().unwrap()[0].clone()
}

fn reload_lines(stderr: &str) -> Vec<&str> {
    stderr
        .lines()
        .filter(|line| line.contains("event=bolt_tls_reload"))
        .collect()
}

// A closed terminal or ssh session sends SIGHUP. With a TLS listener too, it
// is the same clean shutdown as SIGTERM: exit 0, sessions closed, socket
// file removed.
#[test]
fn sighup_shuts_a_tls_server_down_cleanly() {
    let dir = short_dir();
    install_pair(dir.path());
    let path = dir.path().join("bolt.sock");
    let unix = format!("unix://{}", path.display());
    let mut options = vec!["--auth".to_owned(), "none".to_owned()];
    options.extend(tls_flags(dir.path()));
    let options: Vec<&str> = options.iter().map(String::as_str).collect();
    let mut serving = Serving::start(dir.path(), &[&unix, "bolt+s://127.0.0.1:0"], &options);
    let mut client = UnixStream::connect(&path).unwrap();
    client
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    log_on(&mut client);
    let status = serving.signal("-HUP");
    assert!(
        status.success(),
        "SIGHUP must exit 0 after a clean shutdown, got {status:?}; {}",
        serving.stderr()
    );
    assert!(!path.exists(), "socket file survived SIGHUP");
    let mut byte = [0];
    assert_eq!(client.read(&mut byte).unwrap(), 0);
    let stderr = serving.stderr();
    assert!(stderr.contains("event=bolt_disconnected"), "{stderr}");
    assert!(reload_lines(&stderr).is_empty(), "{stderr}");
}

// SIGUSR1 re-reads both files: new connections get the new leaf, and a
// connection made before keeps working on the identity it was admitted with.
// A pair that fails validation leaves the active identity alone and is
// reported once.
#[test]
fn sigusr1_rotates_the_certificate_and_keeps_the_last_good_pair() {
    let dir = short_dir();
    let first = install_pair(dir.path());
    let mut options = vec!["--auth".to_owned(), "none".to_owned()];
    options.extend(tls_flags(dir.path()));
    let options: Vec<&str> = options.iter().map(String::as_str).collect();
    let mut serving = Serving::start(dir.path(), &["bolt+s://127.0.0.1:0"], &options);
    let address = serving.addresses[0].clone();
    assert_eq!(leaf(&address, &[&first]), first);
    let mut pinned = connect(&address, &[&first]);
    log_on(&mut pinned);

    let second = install_pair(dir.path());
    assert_ne!(first, second);
    serving.send_signal("-USR1");
    serving.await_stderr("event=bolt_tls_reloaded");
    assert_eq!(leaf(&address, &[&first, &second]), second);
    // RESET on the connection admitted before the reload.
    send(&mut pinned, &[0xb0, 0x0f]);
    success(&mut pinned);

    let marker = "INVALID-TEST-KEY-CONTENTS";
    fs::write(dir.path().join("key.pem"), marker).unwrap();
    serving.send_signal("-USR1");
    serving.await_stderr("event=bolt_tls_reload_failed");
    assert_eq!(leaf(&address, &[&first, &second]), second);
    send(&mut pinned, &[0xb0, 0x0f]);
    success(&mut pinned);
    // A later good pair is still picked up: the failure did not end reloads.
    let third = install_pair(dir.path());
    serving.send_signal("-USR1");
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while leaf(&address, &[&second, &third]) != third {
        assert!(std::time::Instant::now() < deadline, "{}", serving.stderr());
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(serving.signal("-TERM").success());
    let stderr = serving.stderr();
    let lines = reload_lines(&stderr);
    assert_eq!(
        lines.iter().filter(|l| l.contains("reload_failed")).count(),
        1,
        "{stderr}"
    );
    assert_eq!(lines.len(), 3, "{stderr}");
    assert!(!stderr.contains(marker), "key contents reached the log");
}

// Without a TLS listener there is nothing to reload: SIGUSR1 says so in one
// line and the server keeps serving. (Its default action would end the process.)
#[test]
fn sigusr1_without_a_tls_listener_is_ignored_with_one_log_line() {
    let dir = short_dir();
    let mut serving = Serving::start(dir.path(), &["127.0.0.1:0"], &["--auth", "none"]);
    serving.send_signal("-USR1");
    // The observable event comes first, so the checks below can't pass
    // because the signal was merely still on its way.
    let stderr = serving.await_stderr("event=bolt_tls_reload_ignored reason=no_tls_listener");
    assert_eq!(reload_lines(&stderr).len(), 1, "{stderr}");
    let mut client = TcpStream::connect(&serving.addresses[0]).unwrap();
    client
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    log_on(&mut client);
    send(&mut client, &[0xb0, 0x0f]);
    success(&mut client);
    let status = serving.signal("-TERM");
    assert!(status.success(), "{status:?}; {}", serving.stderr());
}

// stderr can go away while the server runs (a closed terminal, a log pipe
// whose reader died). A log line that can't be written must cost nothing: no
// session thread, no accept loop, no signal handling.
#[test]
fn a_server_whose_stderr_is_gone_keeps_serving_and_still_stops_on_sigterm() {
    let dir = short_dir();
    let path = dir.path().join("bolt.sock");
    let unix = format!("unix://{}", path.display());
    let mut serving = Serving::start_with_piped_stderr(
        dir.path(),
        &["127.0.0.1:0", &unix],
        &["--auth", "none", "--max-connections", "1"],
    );
    serving.close_stderr();
    for _ in 0..3 {
        // Each connection logs (connected, negotiated, authenticated,
        // disconnected) into the closed pipe.
        let mut client = TcpStream::connect(&serving.addresses[0]).unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        log_on(&mut client);
        // Over the cap: the accept thread logs the refusal itself.
        let mut refused = TcpStream::connect(&serving.addresses[0]).unwrap();
        refused
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        assert!(matches!(refused.read(&mut [0]), Ok(0) | Err(_)));
        send(&mut client, &[0xb0, 0x0f]);
        success(&mut client);
        // GOODBYE, then wait for the close so the slot is free again.
        send(&mut client, &[0xb0, 0x02]);
        assert_eq!(client.read(&mut [0]).unwrap(), 0);
        std::thread::sleep(Duration::from_millis(50));
    }
    // The signal thread logs too (nothing to reload) and must survive it.
    serving.send_signal("-USR1");
    std::thread::sleep(Duration::from_millis(200));
    let mut client = UnixStream::connect(&path).unwrap();
    client
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    log_on(&mut client);
    let status = serving.signal("-TERM");
    assert!(status.success(), "SIGTERM returned {status:?}");
    assert!(!path.exists(), "socket file survived SIGTERM");
}
