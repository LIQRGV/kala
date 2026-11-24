//! Loopback listener, bounded connection threads and cooperative shutdown.
use crate::{
    auth::{Accounts, Authenticator, NoAuth, Plugin},
    registry::{Registry, SystemDb},
    session,
};
use std::io;
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
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
    /// Listen endpoint; port zero asks the OS for a free port.
    pub listen: SocketAddr,
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
    /// PackStream nesting limit (at most 32).
    pub max_depth: usize,
}
impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            listen: SocketAddr::from(([127, 0, 0, 1], 7687)),
            advertised_address: None,
            default_db: None,
            insecure_allow_plaintext: false,
            max_connections: 64,
            handshake_timeout: Duration::from_secs(10),
            idle_timeout: Duration::from_secs(600),
            write_timeout: Duration::from_secs(10),
            max_message_bytes: 1 << 20,
            max_depth: 32,
        }
    }
}
/// A bound server; account authentication bootstraps the system world if absent.
pub struct Server {
    listener: TcpListener,
    config: ServerConfig,
    registry: Arc<Registry>,
    auth: Arc<dyn Authenticator>,
}
struct Client {
    socket: TcpStream,
    thread: JoinHandle<()>,
}
impl Server {
    /// Bind a loopback-only, no-auth server and discover its databases.
    /// Never creates, opens or reads `system.kala`, and never serves the
    /// `system` database; writes nothing to `directory`.
    pub fn bind(directory: impl AsRef<Path>, config: ServerConfig) -> io::Result<Self> {
        Self::require_loopback(&config)?;
        Self::bind_inner(directory.as_ref(), config, Arc::new(NoAuth), SystemDb::Skip)
    }
    /// Bind with persisted accounts, bootstrapping root@localhost when needed.
    pub fn bind_accounts(directory: impl AsRef<Path>, config: ServerConfig) -> io::Result<Self> {
        if !config.listen.ip().is_loopback() && !config.insecure_allow_plaintext {
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
        Self::require_loopback(&config)?;
        Self::bind_inner(directory.as_ref(), config, auth, SystemDb::Serve)
    }
    fn require_loopback(config: &ServerConfig) -> io::Result<()> {
        if !config.listen.ip().is_loopback() {
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
        if config.max_connections == 0
            || config.max_message_bytes == 0
            || config.max_depth == 0
            || config.max_depth > 32
            || [
                config.handshake_timeout,
                config.idle_timeout,
                config.write_timeout,
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
        let registry = Arc::new(Registry::open(
            directory,
            config.default_db.clone(),
            system,
        )?);
        let listener = TcpListener::bind(config.listen)?;
        listener.set_nonblocking(true)?;
        Ok(Self {
            listener,
            config,
            registry,
            auth,
        })
    }
    /// The actual endpoint, after resolving port zero.
    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.listener.local_addr()
    }
    /// Accept until `shutdown` is set, then close sockets, cancel queries and
    /// join the bounded client threads. No detached connections survive return.
    pub fn run(self, shutdown: Arc<AtomicBool>) -> io::Result<()> {
        let mut clients = Clients::new();
        let mut accept_failed = LogLimit::new();
        let result = loop {
            clients.reap();
            if shutdown.load(Ordering::Relaxed) {
                break Ok(());
            }
            match self.listener.accept() {
                Ok((stream, _)) => self.admit(stream, &mut clients, &shutdown),
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(5))
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => (),
                Err(e) if accept_error_is_fatal(&e) => break Err(e),
                Err(e) => {
                    // A connection aborted before accept, EMFILE/ENFILE, a
                    // network blip: drop it and keep serving (with a short
                    // pause so fd exhaustion can't spin the loop).
                    accept_failed.log(format_args!(
                        "event=bolt_accept_failed error={:?}",
                        e.kind()
                    ));
                    thread::sleep(Duration::from_millis(50));
                }
            }
        };
        shutdown.store(true, Ordering::Relaxed);
        clients.close();
        result
    }
    /// Gives an accepted connection its session thread, or closes it: over
    /// the connection cap, or when its socket or thread can't be set up.
    fn admit(&self, stream: TcpStream, clients: &mut Clients, shutdown: &Arc<AtomicBool>) {
        if clients.active.len() >= self.config.max_connections {
            let _ = stream.shutdown(Shutdown::Both);
            clients.refused.log(format_args!(
                "event=bolt_connection_refused reason=connection_cap"
            ));
            return;
        }
        // The listener is non-blocking, and on macOS/BSD and
        // Windows an accepted socket inherits that; sessions use
        // blocking reads with timeouts. A failure on one socket
        // (reset by the peer, fd pressure) drops that client
        // only, never the server.
        let prepared = stream
            .set_nonblocking(false)
            .and_then(|()| stream.set_nodelay(true))
            .and_then(|()| stream.try_clone());
        let socket = match prepared {
            Ok(s) => s,
            Err(e) => {
                clients.dropped.log(format_args!(
                    "event=bolt_connection_dropped error={:?}",
                    e.kind()
                ));
                let _ = stream.shutdown(Shutdown::Both);
                return;
            }
        };
        clients.next_id = clients.next_id.wrapping_add(1);
        match self.spawn_session(stream, clients.next_id, shutdown) {
            Ok(thread) => clients.active.push(Client { socket, thread }),
            Err(e) => {
                clients.dropped.log(format_args!(
                    "event=bolt_connection_dropped error={:?}",
                    e.kind()
                ));
                let _ = socket.shutdown(Shutdown::Both);
            }
        }
    }
    /// Starts the thread that serves one connection until it ends.
    fn spawn_session(
        &self,
        stream: TcpStream,
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
                let result = session::serve(stream, id, &shared);
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
            let _ = client.socket.shutdown(Shutdown::Both);
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
    use super::accept_error_is_fatal;
    use std::io::{Error, ErrorKind};

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
