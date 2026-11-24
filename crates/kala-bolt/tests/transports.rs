//! One Bolt conformance suite for TCP, Unix sockets and decorating transports.
#![cfg(unix)]
#[path = "common/client.rs"]
mod client;
mod common;
use client::{ALLIES, Client, map, string};
use kala_bolt::packstream::Value;
use kala_bolt::transport::{ListenAddr, Peer, ShutdownHandle, Transport};
use kala_bolt::{Server, ServerConfig};
use kala_system::system;
use std::fs;
use std::io::{self, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::unix::{
    fs::{FileTypeExt, MetadataExt, PermissionsExt, symlink},
    net::{UnixListener, UnixStream},
};
use std::path::Path;
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug)]
enum Kind {
    Tcp,
    Unix,
    WrappedTcp,
    WrappedUnix,
}
const KINDS: [Kind; 4] = [Kind::Tcp, Kind::WrappedTcp, Kind::Unix, Kind::WrappedUnix];
impl Kind {
    fn address(self, dir: &Path) -> ListenAddr {
        match self {
            Self::Tcp | Self::WrappedTcp => ListenAddr::Tcp("127.0.0.1:0".parse().unwrap()),
            Self::Unix | Self::WrappedUnix => ListenAddr::Unix(dir.join("bolt.sock")),
        }
    }
    fn wrapped(self) -> bool {
        matches!(self, Self::WrappedTcp | Self::WrappedUnix)
    }
}

enum Stream {
    Tcp(TcpStream),
    Unix(UnixStream),
}
impl Stream {
    fn connect(addr: &ListenAddr) -> Self {
        match addr {
            ListenAddr::Tcp(addr) => {
                let stream = TcpStream::connect(addr).unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                stream
                    .set_write_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                Self::Tcp(stream)
            }
            ListenAddr::Unix(path) => {
                let stream = UnixStream::connect(path).unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                stream
                    .set_write_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                Self::Unix(stream)
            }
        }
    }
}
impl Read for Stream {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        match self {
            Self::Tcp(s) => s.read(bytes),
            Self::Unix(s) => s.read(bytes),
        }
    }
}
impl Write for Stream {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        match self {
            Self::Tcp(s) => s.write(bytes),
            Self::Unix(s) => s.write(bytes),
        }
    }
    fn flush(&mut self) -> io::Result<()> {
        match self {
            Self::Tcp(s) => s.flush(),
            Self::Unix(s) => s.flush(),
        }
    }
}

/// Decorates the actual server transport, splitting reads and writes into single
/// bytes while forwarding absolute deadlines and out-of-thread shutdown.
struct SplitTransport {
    inner: Box<dyn Transport>,
    writes: Arc<AtomicUsize>,
}
impl Transport for SplitTransport {
    fn read_until(&mut self, bytes: &mut [u8], deadline: Instant) -> io::Result<usize> {
        let len = bytes.len().min(1);
        self.inner.read_until(&mut bytes[..len], deadline)
    }
    fn write_until(&mut self, bytes: &[u8], deadline: Instant) -> io::Result<usize> {
        self.writes.fetch_add(1, Ordering::Relaxed);
        self.inner
            .write_until(&bytes[..bytes.len().min(1)], deadline)
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

struct Running {
    // The server is joined before this directory is dropped.
    _dir: tempfile::TempDir,
    addrs: Vec<ListenAddr>,
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<io::Result<()>>>,
    writes: Arc<AtomicUsize>,
}
impl Running {
    fn start(kind: Kind, mut config: ServerConfig) -> Self {
        let dir = world_dir();
        config.listen = vec![kind.address(dir.path())];
        Self::in_dir(dir, config, kind.wrapped(), true)
    }
    fn in_dir(dir: tempfile::TempDir, config: ServerConfig, wrap: bool, accounts: bool) -> Self {
        let result = if accounts {
            Server::bind_accounts(dir.path(), config)
        } else {
            Server::bind(dir.path(), config)
        };
        assert!(result.is_ok(), "listener must bind: {:?}", result.err());
        Self::from_server(dir, result.unwrap(), wrap)
    }
    fn from_server(dir: tempfile::TempDir, server: Server, wrap: bool) -> Self {
        let writes = Arc::new(AtomicUsize::new(0));
        let count = writes.clone();
        let server = if wrap {
            server.with_transport_wrapper(move |inner| {
                Box::new(SplitTransport {
                    inner,
                    writes: count.clone(),
                })
            })
        } else {
            server
        };
        let addrs = server.local_addrs().unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let shutdown = stop.clone();
        let worker = thread::spawn(move || server.run(shutdown));
        Self {
            _dir: dir,
            addrs,
            stop,
            worker: Some(worker),
            writes,
        }
    }
    fn connected(&self, index: usize, minor: u8) -> Client<Stream> {
        let mut stream = Stream::connect(&self.addrs[index]);
        common::handshake(&mut stream, minor);
        Client { stream }
    }
    fn ready(&self, index: usize) -> Client<Stream> {
        let mut c = self.connected(index, 4);
        c.hello();
        c.success();
        c.send(0x6a, vec![map(&[("scheme", string("none"))])]);
        c.success();
        c
    }
    fn shutdown(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(worker) = self.worker.take() {
            worker.join().unwrap().unwrap();
        }
    }
}
impl Drop for Running {
    fn drop(&mut self) {
        self.shutdown();
    }
}
fn world_dir() -> tempfile::TempDir {
    // Keep Unix sun_path use independent of a long TMPDIR.
    let dir = tempfile::Builder::new()
        .prefix("kt-")
        .tempdir_in("/tmp")
        .unwrap();
    drop(kala_fixture::load_fixture_to(common::VARROW, &dir.path().join("varrow.kala")).unwrap());
    dir
}
fn login(server: &Running, user: &str, password: &str, minor: u8) -> Client<Stream> {
    let mut c = server.connected(0, minor);
    let token = map(&[
        ("scheme", string("basic")),
        ("principal", string(user)),
        ("credentials", string(password)),
    ]);
    if minor == 0 {
        c.send(0x01, vec![token]);
    } else {
        c.hello();
        c.success();
        c.send(0x6a, vec![token]);
    }
    c
}

#[test]
fn bolt_handshake_and_hello_over_a_unix_socket() {
    let server = Running::start(Kind::Unix, ServerConfig::default());
    assert_eq!(server.ready(0).query(ALLIES).len(), 2);
}

#[test]
fn conformance_versions_hello_logon_query_and_failure_reset() {
    for kind in KINDS {
        let server = Running::start(kind, ServerConfig::default());
        for minor in 0..=4 {
            let mut c = login(&server, "root", "", minor);
            c.success();
            assert_eq!(c.query(ALLIES).len(), 2, "{kind:?} Bolt 5.{minor}");
            c.send(0x10, vec![string("not a valid query"), map(&[]), map(&[])]);
            assert!(matches!(c.recv(), Value::Structure(0x7f, _)));
            c.send(0x10, vec![string(ALLIES), map(&[]), map(&[])]);
            assert_eq!(c.recv(), Value::Structure(0x7e, vec![]));
            c.reset();
            assert_eq!(c.query(ALLIES).len(), 2);
        }
        let mut range = Stream::connect(&server.addrs[0]);
        range
            .write_all(&[
                0x60, 0x60, 0xb0, 0x17, 0, 4, 6, 5, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
            ])
            .unwrap();
        let mut version = [0; 4];
        range.read_exact(&mut version).unwrap();
        assert_eq!(version, [0, 0, 4, 5]);
        let mut unsupported = Stream::connect(&server.addrs[0]);
        unsupported
            .write_all(&[
                0x60, 0x60, 0xb0, 0x17, 0, 0, 0, 6, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
            ])
            .unwrap();
        unsupported.read_exact(&mut version).unwrap();
        assert_eq!(version, [0; 4]);
        common::assert_closed(&mut unsupported);
        if kind.wrapped() {
            assert!(server.writes.load(Ordering::Relaxed) > 100);
        }
    }
}

#[test]
fn conformance_password_accounts_and_wrong_password_refusal() {
    for kind in KINDS {
        let dir = world_dir();
        system::add(
            dir.path(),
            "alice@localhost",
            Some("transport-test-password"),
        )
        .unwrap();
        system::grant(dir.path(), "alice@localhost", "read", "varrow").unwrap();
        let config = ServerConfig {
            listen: vec![kind.address(dir.path())],
            ..ServerConfig::default()
        };
        let server = Running::in_dir(dir, config, kind.wrapped(), true);
        for minor in [0, 4] {
            let mut c = login(&server, "alice", "transport-test-password", minor);
            c.success();
            assert_eq!(c.query(ALLIES).len(), 2);
        }
        let mut denied = login(&server, "alice", "wrong-test-password", 4);
        denied.failure("Neo.ClientError.Security.Unauthorized");
        common::assert_closed(&mut denied.stream);
    }
}

#[test]
fn conformance_message_size_and_depth_limits() {
    for kind in KINDS {
        let server = Running::start(
            kind,
            ServerConfig {
                max_message_bytes: 128,
                ..ServerConfig::default()
            },
        );
        let mut c = server.ready(0);
        // A chunk header is enough: reject before allocating/reading its body.
        c.stream.write_all(&[0, 129]).unwrap();
        c.failure("Neo.ClientError.Request.Invalid");
        common::assert_closed(&mut c.stream);
        let server = Running::start(
            kind,
            ServerConfig {
                max_depth: 4,
                ..ServerConfig::default()
            },
        );
        let mut c = server.ready(0);
        let mut nested = Value::Null;
        for _ in 0..12 {
            nested = Value::List(vec![nested]);
        }
        c.send(
            0x10,
            vec![string(ALLIES), map(&[("nested", nested)]), map(&[])],
        );
        c.failure("Neo.ClientError.Request.Invalid");
        common::assert_closed(&mut c.stream);
    }
}

#[test]
fn conformance_handshake_idle_partial_chunk_and_noop_deadlines() {
    for kind in KINDS {
        let server = Running::start(
            kind,
            ServerConfig {
                handshake_timeout: Duration::from_millis(80),
                idle_timeout: Duration::from_millis(80),
                ..ServerConfig::default()
            },
        );
        let mut incomplete = Stream::connect(&server.addrs[0]);
        incomplete.write_all(&[0x60]).unwrap();
        common::assert_closed(&mut incomplete);
        let mut idle = server.ready(0);
        common::assert_closed(&mut idle.stream);
        let mut partial = server.ready(0);
        partial.stream.write_all(&[0, 10, 0xb1]).unwrap();
        common::assert_closed(&mut partial.stream);
        let mut noop = server.ready(0);
        noop.stream.write_all(&[0, 0, 0, 0]).unwrap();
        common::assert_closed(&mut noop.stream);
    }
}

fn wait_for_slot(addr: &ListenAddr) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        assert!(
            Instant::now() < deadline,
            "connection slot was not reclaimed"
        );
        let mut stream = Stream::connect(addr);
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
        thread::yield_now();
    }
}

#[test]
fn conformance_write_deadline_reclaims_a_blocked_connection() {
    for kind in KINDS {
        let dir = world_dir();
        fs::remove_file(dir.path().join("varrow.kala")).unwrap();
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
        drop(kala_fixture::load_fixture_to(&yaml, &dir.path().join("large.kala")).unwrap());
        let config = ServerConfig {
            listen: vec![kind.address(dir.path())],
            max_connections: 1,
            write_timeout: Duration::from_millis(80),
            ..ServerConfig::default()
        };
        let server = Running::in_dir(dir, config, kind.wrapped(), false);
        let mut blocked = server.ready(0);
        blocked
            .run_empty("// kala:datalog\n[:find ?h ?b :where [?e :name ?h] [:e0 :biography ?b]]");
        blocked.send(0x3f, vec![map(&[("n", Value::Integer(-1))])]);
        // 13 MiB exceeds a kernel send buffer. The old connection stays open
        // and unread while the shared cap proves the write deadline released it.
        wait_for_slot(&server.addrs[0]);
    }
}

#[test]
fn conformance_cap_is_shared_and_shutdown_closes_all_listeners_and_clients() {
    for wrap in [false, true] {
        let dir = world_dir();
        let path = dir.path().join("bolt.sock");
        let config = ServerConfig {
            listen: vec![
                ListenAddr::Tcp("127.0.0.1:0".parse().unwrap()),
                ListenAddr::Unix(path.clone()),
            ],
            max_connections: 2,
            ..ServerConfig::default()
        };
        let mut server = Running::in_dir(dir, config, wrap, true);
        let mut tcp = server.ready(0);
        let mut unix = server.ready(1);
        for addr in &server.addrs {
            common::assert_closed(&mut Stream::connect(addr));
        }
        assert_eq!(tcp.query(ALLIES).len(), 2);
        assert_eq!(unix.query(ALLIES).len(), 2);
        server.shutdown();
        common::assert_closed(&mut tcp.stream);
        common::assert_closed(&mut unix.stream);
        let ListenAddr::Tcp(tcp_addr) = server.addrs[0] else {
            panic!("TCP endpoint")
        };
        assert!(TcpStream::connect(tcp_addr).is_err());
        assert!(!path.exists());
        assert!(UnixStream::connect(path).is_err());
    }
}

#[test]
fn unix_peers_match_only_localhost_accounts() {
    let dir = world_dir();
    for host in ["192.168.1.%", "127.0.0.1", "127.0.0.0/8", "%"] {
        system::add(
            dir.path(),
            &format!("alice@{host}"),
            Some("transport-test-password"),
        )
        .unwrap();
    }
    let config = ServerConfig {
        listen: vec![ListenAddr::Unix(dir.path().join("bolt.sock"))],
        ..ServerConfig::default()
    };
    let server = Running::in_dir(dir, config, false, true);
    let mut c = login(&server, "alice", "transport-test-password", 4);
    c.failure("Neo.ClientError.Security.Unauthorized");
    common::assert_closed(&mut c.stream);
    assert_eq!(server.ready(0).query(ALLIES).len(), 2);
}

#[test]
fn unix_no_auth_and_each_tcp_listener_enforce_loopback_rules() {
    let dir = world_dir();
    let unix = ListenAddr::Unix(dir.path().join("bolt.sock"));
    let server = Server::bind(
        dir.path(),
        ServerConfig {
            listen: vec![unix.clone()],
            ..ServerConfig::default()
        },
    )
    .unwrap();
    let running = Running::from_server(dir, server, false);
    assert_eq!(running.ready(0).query(ALLIES).len(), 2);
    drop(running);
    for reverse in [false, true] {
        let dir = world_dir();
        let path = dir.path().join("bolt.sock");
        let mut listen = vec![
            ListenAddr::Unix(path.clone()),
            ListenAddr::Tcp("0.0.0.0:0".parse().unwrap()),
        ];
        if reverse {
            listen.reverse();
        }
        let config = ServerConfig {
            listen,
            ..ServerConfig::default()
        };
        let error = Server::bind_accounts(dir.path(), config.clone())
            .err()
            .unwrap();
        assert!(error.to_string().contains("loopback"));
        assert!(!path.exists());
        let error = Server::bind(
            dir.path(),
            ServerConfig {
                insecure_allow_plaintext: true,
                ..config
            },
        )
        .err()
        .unwrap();
        assert!(error.to_string().contains("loopback"));
        assert!(!path.exists());
    }
}

#[test]
fn unix_metadata_and_route_use_the_advertised_address_policy() {
    for wrap in [false, true] {
        for advertised in [None, Some("graph.example.test:17687")] {
            let server = Running::start(
                if wrap { Kind::WrappedUnix } else { Kind::Unix },
                ServerConfig {
                    advertised_address: advertised.map(str::to_owned),
                    ..ServerConfig::default()
                },
            );
            let expected = advertised
                .map(str::to_owned)
                .unwrap_or_else(|| server.addrs[0].to_string());
            let mut c = server.ready(0);
            let rows = c.query("SHOW DATABASES YIELD address");
            assert!(!rows.is_empty());
            assert!(rows.iter().all(|row| row == &vec![string(&expected)]));
            c.send(0x66, vec![map(&[]), Value::List(vec![]), map(&[])]);
            if advertised.is_none() {
                let failure = c.failure("Neo.ClientError.Request.Invalid");
                assert_eq!(
                    failure["message"],
                    string("routing over a Unix socket needs --advertised-address")
                );
                c.reset();
                assert_eq!(c.query(ALLIES).len(), 2);
            } else {
                let response = c.success();
                let Value::Map(rt) = &response["rt"] else {
                    panic!("routing table")
                };
                let Value::List(servers) = &rt["servers"] else {
                    panic!("servers")
                };
                assert_eq!(servers.len(), 3);
                for server in servers {
                    let Value::Map(server) = server else {
                        panic!("server")
                    };
                    assert_eq!(server["addresses"], Value::List(vec![string(&expected)]));
                }
            }
        }
    }
}

#[test]
fn unix_permissions_stale_live_and_clean_shutdown() {
    for mode in [0o600, 0o660, 0o666] {
        let dir = world_dir();
        let path = dir.path().join("bolt.sock");
        // Dropping this listener leaves a stale socket entry.
        drop(UnixListener::bind(&path).unwrap());
        let config = ServerConfig {
            listen: vec![ListenAddr::Unix(path.clone())],
            unix_mode: mode,
            ..ServerConfig::default()
        };
        let server = Server::bind(dir.path(), config.clone()).unwrap();
        let stat = fs::symlink_metadata(&path).unwrap();
        assert!(stat.file_type().is_socket());
        assert_eq!(stat.permissions().mode() & 0o777, mode);
        let error = Server::bind(dir.path(), config).err().unwrap();
        assert!(error.to_string().contains("already in use"), "{error}");
        let later = fs::symlink_metadata(&path).unwrap();
        assert_eq!((stat.dev(), stat.ino()), (later.dev(), later.ino()));
        drop(server);
        assert!(!path.exists());
    }
    let dir = world_dir();
    let path = dir.path().join("default.sock");
    let server = Server::bind(
        dir.path(),
        ServerConfig {
            listen: vec![ListenAddr::Unix(path.clone())],
            ..ServerConfig::default()
        },
    )
    .unwrap();
    assert_eq!(
        fs::metadata(path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    drop(server);
}

#[test]
fn unix_refuses_regular_files_and_symlinks_without_changing_them() {
    let dir = world_dir();
    let target = dir.path().join("target");
    fs::write(&target, b"preserve me").unwrap();
    let link = dir.path().join("link");
    symlink(&target, &link).unwrap();
    let stale = dir.path().join("stale.sock");
    drop(UnixListener::bind(&stale).unwrap());
    let socket_link = dir.path().join("socket-link");
    symlink(&stale, &socket_link).unwrap();
    for path in [&target, &link, &socket_link] {
        let stat = fs::symlink_metadata(path).unwrap();
        let error = Server::bind(
            dir.path(),
            ServerConfig {
                listen: vec![ListenAddr::Unix(path.clone())],
                ..ServerConfig::default()
            },
        )
        .err()
        .unwrap();
        assert!(
            error.to_string().contains("socket") || error.to_string().contains("symlink"),
            "{error}"
        );
        let later = fs::symlink_metadata(path).unwrap();
        assert_eq!((stat.dev(), stat.ino()), (later.dev(), later.ino()));
    }
    assert_eq!(fs::read(&target).unwrap(), b"preserve me");
    assert_eq!(fs::read_link(link).unwrap(), target);
    assert!(fs::symlink_metadata(stale).unwrap().file_type().is_socket());
}

#[test]
fn unix_cleanup_preserves_replacements() {
    for replacement_kind in ["file", "socket", "symlink"] {
        let dir = world_dir();
        let path = dir.path().join("bolt.sock");
        let server = Server::bind(
            dir.path(),
            ServerConfig {
                listen: vec![ListenAddr::Unix(path.clone())],
                ..ServerConfig::default()
            },
        )
        .unwrap();
        fs::remove_file(&path).unwrap();
        let target = dir.path().join("replacement-target");
        let replacement = match replacement_kind {
            "socket" => Some(UnixListener::bind(&path).unwrap()),
            "symlink" => {
                fs::write(&target, b"replacement").unwrap();
                symlink(&target, &path).unwrap();
                None
            }
            _ => {
                fs::write(&path, b"replacement").unwrap();
                None
            }
        };
        let stat = fs::symlink_metadata(&path).unwrap();
        drop(server);
        let later = fs::symlink_metadata(&path).unwrap();
        assert_eq!((stat.dev(), stat.ino()), (later.dev(), later.ino()));
        if replacement_kind != "socket" {
            assert_eq!(fs::read(&path).unwrap(), b"replacement");
        }
        if replacement_kind == "symlink" {
            assert_eq!(fs::read_link(&path).unwrap(), target);
        }
        drop(replacement);
    }
}

#[test]
fn failed_multi_listener_bind_rolls_back_tcp_and_unix() {
    let dir = world_dir();
    let socket = dir.path().join("rollback.sock");
    let occupied = dir.path().join("occupied");
    fs::write(&occupied, b"existing").unwrap();
    let probe = TcpListener::bind("127.0.0.1:0").unwrap();
    let tcp = probe.local_addr().unwrap();
    drop(probe);
    let error = Server::bind(
        dir.path(),
        ServerConfig {
            listen: vec![
                ListenAddr::Tcp(tcp),
                ListenAddr::Unix(socket.clone()),
                ListenAddr::Unix(occupied.clone()),
            ],
            ..ServerConfig::default()
        },
    )
    .err()
    .unwrap();
    assert!(
        error.to_string().contains(occupied.to_str().unwrap()),
        "{error}"
    );
    assert!(!socket.exists());
    assert!(TcpListener::bind(tcp).is_ok());
    assert_eq!(fs::read(occupied).unwrap(), b"existing");
}

#[test]
fn invalid_listener_lists_and_unix_modes_are_rejected_before_binding() {
    let dir = world_dir();
    let path = dir.path().join("bolt.sock");
    let unix = ListenAddr::Unix(path.clone());
    for (listen, expected) in [
        (vec![], "1 to 16"),
        (vec![unix.clone(), unix.clone()], "duplicate"),
        (
            (0..17)
                .map(|n| ListenAddr::Unix(dir.path().join(format!("{n}.sock"))))
                .collect(),
            "16",
        ),
    ] {
        let result = Server::bind(
            dir.path(),
            ServerConfig {
                listen,
                ..ServerConfig::default()
            },
        );
        assert!(
            result.is_err(),
            "invalid listener list must fail before binding"
        );
        let error = result.err().unwrap();
        assert!(error.to_string().contains(expected), "{error}");
        assert!(!path.exists());
        for n in 0..17 {
            assert!(!dir.path().join(format!("{n}.sock")).exists());
        }
    }
    for mode in [0o1000, 0o10000, u32::MAX] {
        assert!(
            Server::bind(
                dir.path(),
                ServerConfig {
                    listen: vec![unix.clone()],
                    unix_mode: mode,
                    ..ServerConfig::default()
                }
            )
            .is_err()
        );
        assert!(!path.exists());
    }
}

#[test]
#[cfg(target_os = "linux")]
fn unix_maximum_length_paths_bind_and_replace_stale_sockets() {
    use std::os::unix::ffi::OsStrExt;
    let dir = tempfile::Builder::new()
        .prefix("k-")
        .tempdir_in("/tmp")
        .unwrap();
    let name = "s".repeat(107 - dir.path().as_os_str().as_bytes().len() - 1);
    let path = dir.path().join(name);
    assert_eq!(path.as_os_str().as_bytes().len(), 107);
    let config = ServerConfig {
        listen: vec![ListenAddr::Unix(path.clone())],
        ..ServerConfig::default()
    };
    drop(UnixListener::bind(&path).unwrap());
    let result = Server::bind(dir.path(), config.clone());
    assert!(
        result.is_ok(),
        "valid 107-byte stale socket path must bind: {:?}",
        result.err()
    );
    let server = result.unwrap();
    let error = Server::bind(dir.path(), config).err().unwrap();
    assert!(error.to_string().contains("already in use"), "{error}");
    let mut running = Running::from_server(dir, server, false);
    let mut client = running.ready(0);
    running.shutdown();
    common::assert_closed(&mut client.stream);
    assert!(!path.exists());
}

#[test]
fn unix_live_socket_with_full_backlog_does_not_block_startup() {
    use rustix::net::{self, AddressFamily, SocketAddrUnix, SocketFlags, SocketType};
    let dir = world_dir();
    let path = dir.path().join("full.sock");
    let address = SocketAddrUnix::new(&path).unwrap();
    let flags = SocketFlags::NONBLOCK | SocketFlags::CLOEXEC;
    let listener = net::socket_with(AddressFamily::UNIX, SocketType::STREAM, flags, None).unwrap();
    net::bind(&listener, &address).unwrap();
    net::listen(&listener, 1).unwrap();
    let mut clients = vec![];
    let mut full = false;
    for _ in 0..256 {
        let client =
            net::socket_with(AddressFamily::UNIX, SocketType::STREAM, flags, None).unwrap();
        match net::connect(&client, &address) {
            Ok(()) => clients.push(client),
            Err(rustix::io::Errno::AGAIN | rustix::io::Errno::INPROGRESS) => {
                full = true;
                break;
            }
            Err(error) => panic!("unexpected Unix connect result: {error}"),
        }
    }
    assert!(full, "test must saturate the listen backlog");
    let directory = dir.path().to_owned();
    let config = ServerConfig {
        listen: vec![ListenAddr::Unix(path.clone())],
        ..ServerConfig::default()
    };
    let (tx, rx) = std::sync::mpsc::channel();
    let worker = thread::spawn(move || {
        let _ = tx.send(Server::bind(directory, config).err());
    });
    let error = rx
        .recv_timeout(Duration::from_secs(5))
        .expect("probing a full Unix backlog must not block")
        .expect("a live socket must be refused");
    worker.join().unwrap();
    assert!(error.to_string().contains("already in use"), "{error}");
    assert!(path.exists());
    drop(clients);
    drop(listener);
}

#[test]
fn unix_shared_parent_requires_sticky_protection() {
    for mode in [0o777, 0o770, 0o1777] {
        let dir = world_dir();
        let shared = dir.path().join("shared");
        fs::create_dir(&shared).unwrap();
        fs::set_permissions(&shared, fs::Permissions::from_mode(mode)).unwrap();
        let path = shared.join("bolt.sock");
        let result = Server::bind(
            dir.path(),
            ServerConfig {
                listen: vec![ListenAddr::Unix(path.clone())],
                ..ServerConfig::default()
            },
        );
        if mode & 0o1000 != 0 {
            assert!(
                result.is_ok(),
                "sticky shared parent should bind: {:?}",
                result.err()
            );
            drop(result.unwrap());
        } else {
            assert!(
                result.is_err(),
                "unprotected writable parent must be refused"
            );
            let error = result.err().unwrap();
            assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
            assert!(error.to_string().contains("sticky"), "{error}");
        }
        assert!(!path.exists());
    }
}

#[test]
fn unix_cleanup_pins_parent_across_rename_and_symlink_redirection() {
    let dir = world_dir();
    let original = dir.path().join("original");
    let renamed = dir.path().join("renamed");
    let redirect = dir.path().join("redirect");
    fs::create_dir(&original).unwrap();
    symlink(&original, &redirect).unwrap();
    let path = redirect.join("bolt.sock");
    let server = Server::bind(
        dir.path(),
        ServerConfig {
            listen: vec![ListenAddr::Unix(path)],
            ..ServerConfig::default()
        },
    )
    .unwrap();
    fs::rename(&original, &renamed).unwrap();
    fs::create_dir(&original).unwrap();
    let replacement = original.join("bolt.sock");
    fs::write(&replacement, b"must survive").unwrap();
    assert!(renamed.join("bolt.sock").exists());
    drop(server);
    assert!(!renamed.join("bolt.sock").exists());
    assert_eq!(fs::read(&replacement).unwrap(), b"must survive");
    assert_eq!(fs::read_link(redirect).unwrap(), original);
}
