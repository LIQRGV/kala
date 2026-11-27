//! PR #17 (slice 16) review polish, through the real `kala serve` binary:
//! SIGTERM runs the clean shutdown, and the auth log names the transport.
#![cfg(unix)]

#[path = "common/bolt.rs"]
mod bolt;
#[path = "common/serving.rs"]
mod serving;

use bolt::log_on;
use serving::{Serving, short_dir};
use std::io::Read;
use std::net::TcpStream;
use std::os::unix::net::UnixStream;
use std::time::Duration;

// `systemctl stop` and `docker stop` send SIGTERM, not SIGINT, and a closed
// terminal sends SIGHUP: both must run the same shutdown as Ctrl-C (drain,
// close, remove the socket file) and exit 0.
#[test]
fn sigterm_and_sighup_shut_down_cleanly_and_remove_the_socket() {
    for signal in ["-TERM", "-HUP"] {
        let dir = short_dir();
        let path = dir.path().join("bolt.sock");
        let unix = format!("unix://{}", path.display());
        let mut serving = Serving::start(dir.path(), &[&unix], &["--auth", "none"]);
        assert!(path.exists());
        let mut client = UnixStream::connect(&path).unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        log_on(&mut client);
        let status = serving.signal(signal);
        assert!(
            status.success(),
            "{signal} must exit 0 after a clean shutdown, got {status:?}; {}",
            serving.stderr()
        );
        assert!(!path.exists(), "socket file survived {signal}");
        // The open client was closed by the shutdown, not left to a killed process.
        let mut byte = [0];
        assert_eq!(client.read(&mut byte).unwrap(), 0);
        assert!(
            serving.stderr().contains("event=bolt_disconnected"),
            "{}",
            serving.stderr()
        );
    }
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
