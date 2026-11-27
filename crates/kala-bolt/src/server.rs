//! Loopback listener, bounded connection threads and cooperative shutdown.
use crate::{
    auth::{Accounts, Authenticator, NoAuth, Plugin},
    registry::{Registry, SystemDb},
    session,
    transport::{Accepted, ListenAddr, Listener, ShutdownHandle, Transport},
};
use std::io;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::thread::{self, JoinHandle};
use std::time::Duration;

/// Listener settings. All durations and bounds must be nonzero.
#[derive(Clone, Debug)]
pub struct ServerConfig {
    /// Ordered listen endpoints; TCP port zero asks the OS for a free port.
    pub listen: Vec<ListenAddr>,
    /// Filesystem permissions for Unix sockets.
    pub unix_mode: u32,
    /// Public routing endpoint (HOST:PORT); otherwise the reached socket address.
    pub advertised_address: Option<String>,
    /// Default world name; inferred when exactly one world is registered.
    pub default_db: Option<String>,
    /// Explicitly permit unencrypted account authentication on a non-loopback listener.
    pub insecure_allow_plaintext: bool,
    /// Most simultaneously active client threads.
    pub max_connections: usize,
    /// Deadline for the whole 20-byte handshake.
    pub handshake_timeout: Duration,
    /// Deadline for receiving a complete message, including trickling chunks.
    pub idle_timeout: Duration,
    /// Deadline for writing one response batch.
    pub write_timeout: Duration,
    /// Largest incoming or outgoing message.
    pub max_message_bytes: usize,
    /// Refuse writes before opening a world.
    pub read_only: bool,
    /// Let the server's writable handles acknowledge and cut a world's
    /// damaged last transaction (to a quarantine file) by themselves, so
    /// the world is writable again without an operator. Off by default:
    /// the cut transaction may have been acknowledged to a client, and its
    /// entry IDs and tx number are issued again, so a client holding an ID
    /// from it could `correct` the wrong fact. Never applies to `system`;
    /// ignored (with a startup note) when `read_only` is set.
    pub acknowledge_recovery: bool,
    /// Maximum open explicit write snapshots.
    pub max_write_transactions: usize,
    /// Maximum total lifetime of an explicit transaction.
    pub tx_timeout: Duration,
    /// Maximum idle time between transaction messages.
    pub tx_idle_timeout: Duration,
    /// Maximum wait for a writer or a causal bookmark.
    pub busy_timeout: Duration,
    /// PackStream nesting limit (at most 32).
    pub max_depth: usize,
}
impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            listen: vec![ListenAddr::Tcp(SocketAddr::from(([127, 0, 0, 1], 7687)))],
            unix_mode: 0o600,
            advertised_address: None,
            default_db: None,
            insecure_allow_plaintext: false,
            max_connections: 64,
            handshake_timeout: Duration::from_secs(10),
            idle_timeout: Duration::from_secs(600),
            write_timeout: Duration::from_secs(10),
            max_message_bytes: 1 << 20,
            max_depth: 32,
            read_only: false,
            acknowledge_recovery: false,
            max_write_transactions: 8,
            tx_timeout: Duration::from_secs(60),
            tx_idle_timeout: Duration::from_secs(30),
            busy_timeout: Duration::from_secs(5),
        }
    }
}
/// A bound server; account authentication bootstraps the system world if absent.
pub struct Server {
    listeners: Vec<Listener>,
    wrapper: Option<Arc<TransportWrapper>>,
    /// Per-socket setup of an accepted connection; tests inject failures.
    prepare: fn(Accepted) -> io::Result<Box<dyn Transport>>,
    config: ServerConfig,
    registry: Arc<Registry>,
    auth: Arc<dyn Authenticator>,
}
/// Test hook: see [`Server::open_counts`].
#[cfg(feature = "test-hooks")]
#[doc(hidden)]
pub struct OpenCounts(Arc<Registry>);
#[cfg(feature = "test-hooks")]
impl OpenCounts {
    /// How often the shared read snapshot of `db` was replayed.
    pub fn reader(&self, db: &str) -> usize {
        self.0.opens(db).0
    }
    /// How often a writable handle of `db` was replayed.
    pub fn writer(&self, db: &str) -> usize {
        self.0.opens(db).1
    }
}
type TransportWrapper = dyn Fn(Box<dyn Transport>) -> Box<dyn Transport> + Send + Sync;

struct Client {
    socket: Box<dyn ShutdownHandle>,
    thread: JoinHandle<()>,
}
impl Server {
    /// Bind a loopback-only, no-auth server and discover its databases.
    /// Never creates, opens or reads `system.kala`, and never serves the
    /// `system` database; writes no database files.
    pub fn bind(directory: impl AsRef<Path>, config: ServerConfig) -> io::Result<Self> {
        Self::validate_listeners(&config)?;
        Self::require_loopback(&config)?;
        Self::bind_inner(directory.as_ref(), config, Arc::new(NoAuth), SystemDb::Skip)
    }
    /// Bind with persisted accounts, bootstrapping root@localhost when needed.
    pub fn bind_accounts(directory: impl AsRef<Path>, config: ServerConfig) -> io::Result<Self> {
        Self::validate_listeners(&config)?;
        if config.listen.iter().any(|a| !a.is_loopback()) && !config.insecure_allow_plaintext {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "accounts require a loopback listen address unless --insecure-allow-plaintext is given",
            ));
        }
        if config.insecure_allow_plaintext {
            eprintln!(
                "kala serve: WARNING: passwords travel unencrypted; --insecure-allow-plaintext is enabled"
            );
        }
        let directory = directory.as_ref();
        if crate::system::ensure(directory).map_err(io::Error::other)? {
            eprintln!("kala serve: created system.kala with root@localhost");
        }
        for account in crate::system::list(directory).map_err(io::Error::other)? {
            if account.plugin == Plugin::Trust {
                eprintln!(
                    "kala serve: account {}@{} uses trust (no password)",
                    account.user, account.host
                );
            }
        }
        let auth = Accounts::open(directory).map_err(io::Error::other)?;
        Self::bind_inner(directory, config, Arc::new(auth), SystemDb::Serve)
    }
    /// Supply an authenticator on a loopback-only listener.
    pub fn bind_with_auth(
        directory: impl AsRef<Path>,
        config: ServerConfig,
        auth: Arc<dyn Authenticator>,
    ) -> io::Result<Self> {
        Self::validate_listeners(&config)?;
        Self::require_loopback(&config)?;
        Self::bind_inner(directory.as_ref(), config, auth, SystemDb::Serve)
    }
    fn validate_listeners(config: &ServerConfig) -> io::Result<()> {
        if config.listen.is_empty() || config.listen.len() > 16 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "expected 1 to 16 listeners",
            ));
        }
        if config.unix_mode > 0o777 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Unix mode must be octal permissions 0000 through 0777",
            ));
        }
        let mut seen = std::collections::HashSet::new();
        for addr in &config.listen {
            addr.validate()
                .map_err(|e| io::Error::new(e.kind(), format!("listener {addr}: {e}")))?;
            if !seen.insert(addr) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("duplicate listen address: {addr}"),
                ));
            }
        }
        Ok(())
    }
    fn require_loopback(config: &ServerConfig) -> io::Result<()> {
        if config.listen.iter().any(|a| !a.is_loopback()) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "no-auth server requires a loopback listen address",
            ));
        }
        Ok(())
    }
    fn bind_inner(
        directory: &Path,
        config: ServerConfig,
        auth: Arc<dyn Authenticator>,
        system: SystemDb,
    ) -> io::Result<Self> {
        if config
            .advertised_address
            .as_deref()
            .is_some_and(|s| !valid_advertised_address(s))
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "advertised address must be HOST:PORT or [IPv6]:PORT",
            ));
        }
        if std::time::Instant::now()
            .checked_add(config.busy_timeout)
            .is_none()
            || config.max_connections == 0
            || config.max_message_bytes == 0
            || config.max_depth == 0
            || config.max_depth > 32
            || [
                config.handshake_timeout,
                config.idle_timeout,
                config.write_timeout,
                config.tx_timeout,
                config.tx_idle_timeout,
            ]
            .iter()
            .any(|d| d.is_zero() || std::time::Instant::now().checked_add(*d).is_none())
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid Bolt resource limits",
            ));
        }
        // The server directory is never created, in any auth mode.
        if !directory.is_dir() {
            return Err(io::Error::new(io::ErrorKind::NotFound, "no such directory"));
        }
        if config.acknowledge_recovery && config.read_only {
            eprintln!(
                "kala serve: --acknowledge-recovery is ignored: a read-only server never writes, so it never cuts a damaged tail"
            );
        } else if config.acknowledge_recovery {
            eprintln!(
                "kala serve: WARNING: --acknowledge-recovery is on: a world's damaged last transaction is cut to a quarantine file without asking, and its entry IDs are issued again"
            );
        }
        let registry = Arc::new(
            Registry::open(directory, config.default_db.clone(), system)?
                .acknowledging_recovery(config.acknowledge_recovery && !config.read_only),
        );
        let mut listeners = Vec::new();
        for addr in &config.listen {
            listeners.push(
                Listener::bind(addr, config.unix_mode)
                    .map_err(|e| io::Error::new(e.kind(), format!("listener {addr}: {e}")))?,
            );
        }
        Ok(Self {
            listeners,
            wrapper: None,
            prepare: Accepted::into_transport,
            config,
            registry,
            auth,
        })
    }
    /// Test hook: a counter of how often each kind of handle of a database
    /// was replayed from its file, readable while the server runs.
    #[cfg(feature = "test-hooks")]
    #[doc(hidden)]
    pub fn open_counts(&self) -> OpenCounts {
        OpenCounts(Arc::clone(&self.registry))
    }
    /// Actual bound endpoints, in configuration order.
    pub fn local_addrs(&self) -> io::Result<Vec<ListenAddr>> {
        self.listeners.iter().map(Listener::local_addr).collect()
    }
    /// Wrap every accepted byte transport before running the shared session.
    /// Wrappers must preserve deadlines and cancellation. The peer used for
    /// account matching and the reached address are taken from the accepted
    /// connection before it is wrapped, so a wrapper's own `peer` and
    /// `local_addr` answers are never used.
    pub fn with_transport_wrapper(
        mut self,
        wrapper: impl Fn(Box<dyn Transport>) -> Box<dyn Transport> + Send + Sync + 'static,
    ) -> Self {
        self.wrapper = Some(Arc::new(wrapper));
        self
    }
    /// Accept until `shutdown` is set, then close sockets, cancel queries and
    /// join the bounded client threads. No detached connections survive return.
    pub fn run(self, shutdown: Arc<AtomicBool>) -> io::Result<()> {
        let mut clients = Clients::new();
        let mut accept_failed = LogLimit::new();
        let result = 'accept: loop {
            clients.reap();
            if shutdown.load(Ordering::Relaxed) {
                break Ok(());
            }
            let mut accepted = false;
            for listener in &self.listeners {
                match listener.accept() {
                    Ok(stream) => {
                        accepted = true;
                        self.admit(stream, &mut clients, &shutdown);
                    }
                    Err(e)
                        if matches!(
                            e.kind(),
                            io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                        ) => {}
                    Err(e) if accept_error_is_fatal(&e) => break 'accept Err(e),
                    Err(e) => {
                        accept_failed.log(format_args!(
                            "event=bolt_accept_failed error={:?}",
                            e.kind()
                        ));
                        thread::sleep(Duration::from_millis(50));
                    }
                }
            }
            if !accepted {
                thread::sleep(Duration::from_millis(5));
            }
        };
        shutdown.store(true, Ordering::Relaxed);
        clients.close();
        result
    }
    /// Gives an accepted connection its session thread, or closes it: over
    /// the connection cap, or when its socket or thread can't be set up.
    /// Nothing that goes wrong with one connection reaches the accept loop.
    fn admit(&self, accepted: Accepted, clients: &mut Clients, shutdown: &Arc<AtomicBool>) {
        if clients.active.len() >= self.config.max_connections {
            accepted.close();
            clients.refused.log(format_args!(
                "event=bolt_connection_refused reason=connection_cap"
            ));
            return;
        }
        let stream = match (self.prepare)(accepted) {
            Ok(stream) => stream,
            Err(e) => {
                // The failed socket was dropped (closed) with the error.
                clients.dropped.log(format_args!(
                    "event=bolt_connection_dropped error={:?}",
                    e.kind()
                ));
                return;
            }
        };
        // Who connected, and where, is asked of the real connection before
        // any wrapper sees it: a wrapper frames bytes and must not be able to
        // turn a TCP client into a `localhost` peer.
        let origin = stream.peer().and_then(|peer| {
            Ok(session::Origin {
                peer,
                reached: stream.local_addr()?,
            })
        });
        let origin = match origin {
            Ok(origin) => origin,
            Err(e) => {
                clients.dropped.log(format_args!(
                    "event=bolt_connection_dropped error={:?}",
                    e.kind()
                ));
                return;
            }
        };
        let stream = match &self.wrapper {
            Some(wrap) => wrap(stream),
            None => stream,
        };
        let prepared = stream.shutdown_handle();
        let socket = match prepared {
            Ok(s) => s,
            Err(e) => {
                clients.dropped.log(format_args!(
                    "event=bolt_connection_dropped error={:?}",
                    e.kind()
                ));
                if let Ok(handle) = stream.shutdown_handle() {
                    let _ = handle.shutdown();
                }
                return;
            }
        };
        clients.next_id = clients.next_id.wrapping_add(1);
        match self.spawn_session(stream, origin, clients.next_id, shutdown) {
            Ok(thread) => clients.active.push(Client { socket, thread }),
            Err(e) => {
                clients.dropped.log(format_args!(
                    "event=bolt_connection_dropped error={:?}",
                    e.kind()
                ));
                let _ = socket.shutdown();
            }
        }
    }
    /// Starts the thread that serves one connection until it ends.
    fn spawn_session(
        &self,
        stream: Box<dyn Transport>,
        origin: session::Origin,
        id: u64,
        shutdown: &Arc<AtomicBool>,
    ) -> io::Result<JoinHandle<()>> {
        let registry = Arc::clone(&self.registry);
        let auth = Arc::clone(&self.auth);
        let config = self.config.clone();
        let stop = Arc::clone(shutdown);
        thread::Builder::new()
            .name(format!("bolt-{id}"))
            .spawn(move || {
                eprintln!("event=bolt_connected connection_id={id}");
                let shared = session::Shared {
                    registry: &registry,
                    auth: auth.as_ref(),
                    config: &config,
                    stop: &stop,
                };
                let result = session::serve(stream, origin, id, &shared);
                eprintln!(
                    "event=bolt_disconnected connection_id={id} reason={:?}",
                    result.err().map(|e| e.kind())
                );
            })
    }
}

/// The connections being served, and the accept loop's rate-limited logs
/// about the ones it turned away.
struct Clients {
    active: Vec<Client>,
    /// The last connection ID given out.
    next_id: u64,
    refused: LogLimit,
    dropped: LogLimit,
}

impl Clients {
    fn new() -> Self {
        Self {
            active: Vec::new(),
            next_id: 0,
            refused: LogLimit::new(),
            dropped: LogLimit::new(),
        }
    }

    /// Joins the threads of connections that have ended.
    fn reap(&mut self) {
        let mut i = 0;
        while i < self.active.len() {
            if self.active[i].thread.is_finished() {
                let client = self.active.swap_remove(i);
                let _ = client.thread.join();
            } else {
                i += 1;
            }
        }
    }

    /// Closes every socket, then joins every thread.
    fn close(self) {
        for client in &self.active {
            let _ = client.socket.shutdown();
        }
        for client in self.active {
            let _ = client.thread.join();
        }
    }
}

/// Whether an `accept` error means the listener itself is broken (stop the
/// server) rather than one failed connection attempt (keep serving).
fn accept_error_is_fatal(e: &io::Error) -> bool {
    // By errno where there is one: only EBADF, ENOTSOCK and EINVAL mean the
    // listening socket is unusable. EPERM (a firewall rule refusing a peer),
    // EMFILE/ENFILE and the accept(2) "retry" errors are per connection.
    #[cfg(target_os = "linux")]
    const BROKEN: [i32; 3] = [9, 88, 22];
    #[cfg(any(target_os = "macos", target_os = "ios", target_os = "freebsd"))]
    const BROKEN: [i32; 3] = [9, 38, 22];
    #[cfg(all(
        unix,
        not(any(
            target_os = "linux",
            target_os = "macos",
            target_os = "ios",
            target_os = "freebsd"
        ))
    ))]
    const BROKEN: [i32; 0] = [];
    #[cfg(unix)]
    if let Some(errno) = e.raw_os_error() {
        return BROKEN.contains(&errno);
    }
    e.kind() == io::ErrorKind::InvalidInput
}

/// Coalesces a repeating log event: the first occurrence in each second is
/// printed, later ones are counted and reported once with the next print,
/// so a client looping connect/close can't flood stderr.
struct LogLimit {
    last: Option<std::time::Instant>,
    suppressed: u64,
}

impl LogLimit {
    const fn new() -> Self {
        Self {
            last: None,
            suppressed: 0,
        }
    }

    fn log(&mut self, line: std::fmt::Arguments<'_>) {
        let now = std::time::Instant::now();
        if self
            .last
            .is_some_and(|t| now.duration_since(t) < Duration::from_secs(1))
        {
            self.suppressed += 1;
            return;
        }
        if self.suppressed > 0 {
            eprintln!("{line} suppressed_since_last={}", self.suppressed);
        } else {
            eprintln!("{line}");
        }
        self.last = Some(now);
        self.suppressed = 0;
    }
}

fn valid_advertised_address(address: &str) -> bool {
    if address.len() > 1024 || address.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return false;
    }
    let Some((host, port)) = address.rsplit_once(':') else {
        return false;
    };
    if port.parse::<u16>().ok().is_none_or(|p| p == 0) {
        return false;
    }
    if host.starts_with('[') {
        return host
            .strip_prefix('[')
            .and_then(|s| s.strip_suffix(']'))
            .is_some_and(|s| s.parse::<std::net::Ipv6Addr>().is_ok());
    }
    !host.is_empty()
        && host
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'.' | b'-' | b'_'))
}

#[cfg(test)]
mod tests {
    use super::{Server, ServerConfig, accept_error_is_fatal};
    use crate::transport::{Accepted, ListenAddr, Transport};
    use std::io::{Error, ErrorKind, Read, Write};
    use std::net::TcpStream;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::time::Duration;

    static PREPARED: AtomicUsize = AtomicUsize::new(0);
    /// The first accepted socket fails its setup the way a client that reset
    /// right after connecting does on macOS: setsockopt returns EINVAL, which
    /// `accept_error_is_fatal` treats as a broken listener.
    fn first_setup_fails(accepted: Accepted) -> std::io::Result<Box<dyn Transport>> {
        if PREPARED.fetch_add(1, Ordering::SeqCst) == 0 {
            return Err(Error::from_raw_os_error(22));
        }
        accepted.into_transport()
    }

    // PR #17 polish: per-socket setup is not an accept error. One client whose
    // socket can't be set up is dropped; the server keeps accepting.
    #[test]
    fn a_failed_socket_setup_drops_only_that_client() {
        let dir = tempfile::tempdir().unwrap();
        let config = ServerConfig {
            listen: vec![ListenAddr::Tcp("127.0.0.1:0".parse().unwrap())],
            ..ServerConfig::default()
        };
        let mut server = Server::bind(dir.path(), config).unwrap();
        server.prepare = first_setup_fails;
        let ListenAddr::Tcp(addr) = server.local_addrs().unwrap()[0].clone() else {
            panic!("TCP listener")
        };
        let stop = Arc::new(AtomicBool::new(false));
        let shutdown = Arc::clone(&stop);
        let worker = std::thread::spawn(move || server.run(shutdown));
        let connect = || {
            let stream = TcpStream::connect(addr)?;
            stream.set_read_timeout(Some(Duration::from_secs(5)))?;
            Ok::<_, Error>(stream)
        };
        // The unlucky client is closed without a byte.
        let mut dropped = connect().unwrap();
        let mut byte = [0; 1];
        assert!(matches!(dropped.read(&mut byte), Ok(0) | Err(_)));
        // The next client is served: the handshake answers Bolt 5.4.
        let served = connect().and_then(|mut next| {
            next.write_all(&[
                0x60, 0x60, 0xb0, 0x17, 0, 0, 4, 5, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
            ])?;
            let mut version = [0; 4];
            next.read_exact(&mut version)?;
            Ok(version)
        });
        stop.store(true, Ordering::SeqCst);
        let result = worker.join().unwrap();
        assert!(
            result.is_ok(),
            "one client's setup failure stopped the server: {result:?}"
        );
        assert_eq!(served.unwrap(), [0, 0, 4, 5]);
    }

    // PR #4 review round 1: per-connection accept failures never stop the
    // server; only a broken listener does.
    #[test]
    fn only_listener_errors_stop_the_accept_loop() {
        for kind in [
            ErrorKind::ConnectionAborted,
            ErrorKind::ConnectionReset,
            ErrorKind::OutOfMemory,
            ErrorKind::Other,
            ErrorKind::TimedOut,
            // Review round 2: EPERM from accept(2) is a firewall rule
            // refusing one peer, not a broken listener.
            ErrorKind::PermissionDenied,
        ] {
            assert!(!accept_error_is_fatal(&Error::from(kind)), "{kind:?}");
        }
        // EMFILE/ENFILE, EPERM, and the accept(2) "retry" errors.
        for errno in [24, 23, 1, 13, 100, 71, 92, 112, 64, 113, 101] {
            assert!(
                !accept_error_is_fatal(&Error::from_raw_os_error(errno)),
                "{errno}"
            );
        }
        // A broken listener: EBADF, ENOTSOCK, EINVAL (Linux numbers).
        #[cfg(target_os = "linux")]
        for errno in [9, 88, 22] {
            assert!(
                accept_error_is_fatal(&Error::from_raw_os_error(errno)),
                "{errno}"
            );
        }
    }
}
