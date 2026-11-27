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
            generation: 0,
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

// ---- The Unix socket's lifetime lock file ----

use std::fs;
use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt, symlink};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::Barrier;

fn unix_config(path: &Path) -> ServerConfig {
    ServerConfig {
        listen: vec![ListenAddr::Unix(path.to_owned())],
        ..ServerConfig::default()
    }
}

fn lock_file(socket: &Path) -> PathBuf {
    let name = socket.file_name().unwrap().to_str().unwrap();
    socket.with_file_name(format!(".{name}.lock"))
}

// Anyone who can open the parent directory read-only (0755, /tmp) can flock
// it. That must not stop a server from starting or from removing its socket.
#[test]
fn a_flock_on_the_parent_directory_blocks_neither_start_nor_cleanup() {
    use rustix::fs::{FlockOperation, flock};
    for operation in [
        FlockOperation::NonBlockingLockShared,
        FlockOperation::NonBlockingLockExclusive,
    ] {
        let dir = world_dir();
        let path = dir.path().join("bolt.sock");
        let held = fs::File::open(dir.path()).unwrap();
        flock(&held, operation).unwrap();
        let server = Server::bind(dir.path(), unix_config(&path));
        assert!(
            server.is_ok(),
            "a directory flock blocked the start: {:?}",
            server.err()
        );
        assert!(fs::symlink_metadata(&path).unwrap().file_type().is_socket());
        drop(server);
        assert!(!path.exists(), "a directory flock blocked the cleanup");
        drop(held);
    }
}

// Cleanup on its own: the directory is locked only after the server started.
#[test]
fn a_flock_taken_on_the_parent_directory_later_does_not_block_cleanup() {
    use rustix::fs::{FlockOperation, flock};
    let dir = world_dir();
    let path = dir.path().join("bolt.sock");
    let server = Server::bind(dir.path(), unix_config(&path)).unwrap();
    let held = fs::File::open(dir.path()).unwrap();
    flock(&held, FlockOperation::NonBlockingLockShared).unwrap();
    drop(server);
    assert!(!path.exists(), "a directory flock blocked the cleanup");
    drop(held);
}

#[test]
fn simultaneous_starts_on_one_path_have_exactly_one_winner() {
    for _ in 0..3 {
        let dir = world_dir();
        let path = dir.path().join("bolt.sock");
        let barrier = Barrier::new(2);
        let results: Vec<io::Result<Server>> = thread::scope(|scope| {
            let starts: Vec<_> = (0..2)
                .map(|_| {
                    scope.spawn(|| {
                        barrier.wait();
                        Server::bind(dir.path(), unix_config(&path))
                    })
                })
                .collect();
            // Each winner stays bound until both attempts have finished.
            starts.into_iter().map(|s| s.join().unwrap()).collect()
        });
        let errors: Vec<String> = results
            .iter()
            .filter_map(|r| r.as_ref().err().map(ToString::to_string))
            .collect();
        assert_eq!(errors.len(), 1, "exactly one start must lose: {errors:?}");
        assert!(
            errors[0].contains("is already in use by another kala serve"),
            "{}",
            errors[0]
        );
        assert!(
            errors[0].contains(lock_file(&path).to_str().unwrap()),
            "{}",
            errors[0]
        );
        // The winner's socket was not disturbed by the loser.
        assert!(UnixStream::connect(&path).is_ok());
        drop(results);
        assert!(!path.exists());
    }
}

#[test]
fn servers_on_different_sockets_in_one_directory_start_and_stop_concurrently() {
    const SERVERS: usize = 4;
    let dir = world_dir();
    for round in 0..20 {
        let barrier = Barrier::new(SERVERS);
        thread::scope(|scope| {
            for n in 0..SERVERS {
                let (barrier, dir) = (&barrier, dir.path());
                scope.spawn(move || {
                    let path = dir.join(format!("{n}.sock"));
                    barrier.wait();
                    let server = Server::bind(dir, unix_config(&path));
                    assert!(
                        server.is_ok(),
                        "round {round}, server {n}: {:?}",
                        server.err()
                    );
                    assert!(path.exists());
                    drop(server);
                    assert!(!path.exists(), "round {round}, server {n}: socket left");
                });
            }
        });
    }
}

// `--unix.mode 0000` (or 0060) leaves a socket its own server cannot connect
// to. After a crash the next start must still reclaim it.
#[test]
fn a_leftover_socket_we_cannot_probe_is_reclaimed() {
    for mode in [0o000, 0o060] {
        let dir = world_dir();
        let path = dir.path().join("bolt.sock");
        drop(UnixListener::bind(&path).unwrap());
        fs::set_permissions(&path, fs::Permissions::from_mode(mode)).unwrap();
        let server = Server::bind(dir.path(), unix_config(&path));
        assert!(
            server.is_ok(),
            "leftover {mode:04o} socket: {:?}",
            server.err()
        );
        let stat = fs::symlink_metadata(&path).unwrap();
        assert!(stat.file_type().is_socket());
        assert_eq!(stat.permissions().mode() & 0o777, 0o600);
        drop(server);
        assert!(!path.exists());
    }
}

// The lock only decides Kala against Kala. A live listener that is not a
// Kala server holds no lock, and the connect probe still protects it.
#[test]
fn a_live_listener_without_the_lock_is_still_in_use() {
    let dir = world_dir();
    let path = dir.path().join("bolt.sock");
    let other = UnixListener::bind(&path).unwrap();
    let before = fs::symlink_metadata(&path).unwrap();
    let error = Server::bind(dir.path(), unix_config(&path)).err().unwrap();
    assert!(error.to_string().contains("already in use"), "{error}");
    let after = fs::symlink_metadata(&path).unwrap();
    assert_eq!((before.dev(), before.ino()), (after.dev(), after.ino()));
    drop(other);
}

#[test]
fn the_lock_file_is_private_and_removed_with_the_socket() {
    let dir = world_dir();
    let path = dir.path().join("bolt.sock");
    let lock = lock_file(&path);
    let server = Server::bind(dir.path(), unix_config(&path)).unwrap();
    let stat = fs::symlink_metadata(&lock).unwrap();
    assert!(stat.file_type().is_file());
    assert_eq!(stat.permissions().mode() & 0o7777, 0o600);
    assert_eq!(stat.len(), 0);
    drop(server);
    assert!(!path.exists());
    assert!(!lock.exists(), "lock file left after a clean shutdown");
    // A crash leaves both files behind; the next start reuses the lock file.
    drop(UnixListener::bind(&path).unwrap());
    fs::write(&lock, b"").unwrap();
    let server = Server::bind(dir.path(), unix_config(&path)).unwrap();
    assert!(fs::symlink_metadata(&path).unwrap().file_type().is_socket());
    drop(server);
    assert!(!path.exists());
    assert!(!lock.exists());
}

#[test]
fn a_symlink_or_non_regular_file_at_the_lock_name_is_refused_untouched() {
    for kind in ["symlink", "dangling symlink", "directory"] {
        let dir = world_dir();
        let path = dir.path().join("bolt.sock");
        let lock = lock_file(&path);
        let target = dir.path().join("target");
        match kind {
            "symlink" => {
                fs::write(&target, b"preserve me").unwrap();
                symlink(&target, &lock).unwrap();
            }
            "dangling symlink" => symlink(&target, &lock).unwrap(),
            _ => fs::create_dir(&lock).unwrap(),
        }
        let before = fs::symlink_metadata(&lock).unwrap();
        let result = Server::bind(dir.path(), unix_config(&path));
        assert!(result.is_err(), "{kind} at the lock name must be refused");
        let error = result.err().unwrap().to_string();
        assert!(error.contains(lock.to_str().unwrap()), "{kind}: {error}");
        assert!(error.contains("lock file"), "{kind}: {error}");
        let after = fs::symlink_metadata(&lock).unwrap();
        assert_eq!((before.dev(), before.ino()), (after.dev(), after.ino()));
        assert_eq!(before.file_type(), after.file_type());
        assert!(!path.exists(), "{kind}: a socket was published");
        match kind {
            "symlink" => assert_eq!(fs::read(&target).unwrap(), b"preserve me"),
            "dangling symlink" => assert!(!target.exists(), "the lock was created through"),
            _ => assert!(fs::read_dir(&lock).unwrap().next().is_none()),
        }
    }
}

// The lock file is removed at shutdown while the lock is still held, and a
// start re-checks that the file it locked is still the one at the name. Under
// start/stop churn on one path that must never let two servers hold it.
#[test]
fn start_stop_churn_on_one_path_never_has_two_holders() {
    use std::sync::atomic::AtomicUsize;
    const THREADS: usize = 4;
    let dir = world_dir();
    let path = dir.path().join("bolt.sock");
    let holders = AtomicUsize::new(0);
    let wins = AtomicUsize::new(0);
    let barrier = Barrier::new(THREADS);
    thread::scope(|scope| {
        for _ in 0..THREADS {
            scope.spawn(|| {
                barrier.wait();
                for _ in 0..25 {
                    match Server::bind(dir.path(), unix_config(&path)) {
                        Ok(server) => {
                            assert_eq!(
                                holders.fetch_add(1, Ordering::SeqCst),
                                0,
                                "two servers hold one socket path"
                            );
                            wins.fetch_add(1, Ordering::SeqCst);
                            assert!(UnixStream::connect(&path).is_ok());
                            holders.fetch_sub(1, Ordering::SeqCst);
                            drop(server);
                        }
                        Err(error) => assert!(
                            error
                                .to_string()
                                .contains("is already in use by another kala serve"),
                            "{error}"
                        ),
                    }
                }
            });
        }
    });
    assert!(wins.load(Ordering::SeqCst) >= THREADS);
    assert!(!path.exists());
    assert!(!lock_file(&path).exists());
}
