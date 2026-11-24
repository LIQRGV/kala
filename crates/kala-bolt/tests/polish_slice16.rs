//! PR #17 (slice 16) review polish: peer identity behind wrappers and the
//! Unix socket's lifetime lock file.
#![cfg(unix)]
#[path = "common/client.rs"]
mod client;
mod common;
use client::{ALLIES, Client, map, string};
use kala_bolt::auth::{AuthFailure, AuthToken, Authenticator, Identity};
use kala_bolt::packstream::Value;
use kala_bolt::transport::{ListenAddr, Peer, ShutdownHandle, Transport};
use kala_bolt::{Server, ServerConfig};
use std::io;
use std::net::{IpAddr, TcpStream};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

fn world_dir() -> tempfile::TempDir {
    // Keep Unix sun_path use independent of a long TMPDIR.
    let dir = tempfile::Builder::new()
        .prefix("kp-")
        .tempdir_in("/tmp")
        .unwrap();
    drop(kala_fixture::load_fixture_to(common::VARROW, &dir.path().join("varrow.kala")).unwrap());
    dir
}

struct Serving {
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<io::Result<()>>>,
}
impl Serving {
    fn run(server: Server) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let shutdown = stop.clone();
        let worker = Some(thread::spawn(move || server.run(shutdown)));
        Self { stop, worker }
    }
}
impl Drop for Serving {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(worker) = self.worker.take() {
            let result = worker.join().unwrap();
            if !thread::panicking() {
                result.unwrap();
            }
        }
    }
}

/// A wrapper that claims every connection is a local (Unix) peer reached at
/// a Unix address, whatever the real transport is.
struct Lying(Box<dyn Transport>);
impl Transport for Lying {
    fn read_until(&mut self, bytes: &mut [u8], deadline: Instant) -> io::Result<usize> {
        self.0.read_until(bytes, deadline)
    }
    fn write_until(&mut self, bytes: &[u8], deadline: Instant) -> io::Result<usize> {
        self.0.write_until(bytes, deadline)
    }
    fn shutdown_handle(&self) -> io::Result<Box<dyn ShutdownHandle>> {
        self.0.shutdown_handle()
    }
    fn peer(&self) -> io::Result<Peer> {
        Ok(Peer::Local)
    }
    fn local_addr(&self) -> io::Result<ListenAddr> {
        Ok(ListenAddr::Unix("/lie.sock".into()))
    }
}

/// Accepts everyone and records the peer the server reported.
#[derive(Default)]
struct Recording(Mutex<Vec<Peer>>);
impl Authenticator for Recording {
    fn authenticate(&self, token: &AuthToken, peer: IpAddr) -> Result<Identity, AuthFailure> {
        self.authenticate_peer(token, Peer::Ip(peer))
    }
    fn authenticate_peer(&self, _: &AuthToken, peer: Peer) -> Result<Identity, AuthFailure> {
        self.0.lock().unwrap().push(peer);
        Ok(Identity {
            principal: "recorded".into(),
            host: String::new(),
            grants: vec!["read:*".into()],
        })
    }
}

// A wrapper (TLS and WebSocket will be wrappers) frames bytes; it must not be
// able to turn a TCP client into a `localhost` peer, which `trust` accounts
// and the Unix-only rules key on.
#[test]
fn a_wrapper_cannot_change_the_peer_or_the_reached_address() {
    let dir = world_dir();
    let auth = Arc::new(Recording::default());
    let config = ServerConfig {
        listen: vec![ListenAddr::Tcp("127.0.0.1:0".parse().unwrap())],
        ..ServerConfig::default()
    };
    let server = Server::bind_with_auth(dir.path(), config, auth.clone())
        .unwrap()
        .with_transport_wrapper(|inner| Box::new(Lying(inner)));
    let ListenAddr::Tcp(addr) = server.local_addrs().unwrap()[0].clone() else {
        panic!("TCP listener")
    };
    let _serving = Serving::run(server);
    let mut stream = TcpStream::connect(addr).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    common::handshake(&mut stream, 4);
    let mut c = Client { stream };
    c.hello();
    c.success();
    c.send(0x6a, vec![map(&[("scheme", string("none"))])]);
    c.success();
    assert_eq!(
        *auth.0.lock().unwrap(),
        [Peer::Ip("127.0.0.1".parse().unwrap())],
        "the authenticator must see the real TCP peer"
    );
    assert_eq!(c.query(ALLIES).len(), 2);
    // The reached address is the real TCP one, not the wrapper's claim.
    let rows = c.query("SHOW DATABASES YIELD address");
    assert!(!rows.is_empty());
    assert!(
        rows.iter()
            .all(|row| row == &vec![string(&addr.to_string())]),
        "{rows:?}"
    );
    // ROUTE answers with a table; the Unix-only refusal is for real local peers.
    c.send(0x66, vec![map(&[]), Value::List(vec![]), map(&[])]);
    let response = c.success();
    let Value::Map(rt) = &response["rt"] else {
        panic!("routing table")
    };
    let Value::List(servers) = &rt["servers"] else {
        panic!("servers")
    };
    for server in servers {
        let Value::Map(server) = server else {
            panic!("server")
        };
        assert_eq!(
            server["addresses"],
            Value::List(vec![string(&addr.to_string())])
        );
    }
}
