//! Loopback listener, bounded connection threads and cooperative shutdown.
use crate::{
    auth::{Authenticator, NoAuth},
    registry::Registry,
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
    /// Loopback endpoint; port zero asks the OS for a free port.
    pub listen: SocketAddr,
    /// Default world name; inferred when exactly one world is registered.
    pub default_db: Option<String>,
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
            default_db: None,
            max_connections: 64,
            handshake_timeout: Duration::from_secs(10),
            idle_timeout: Duration::from_secs(600),
            write_timeout: Duration::from_secs(10),
            max_message_bytes: 1 << 20,
            max_depth: 32,
        }
    }
}
/// A bound server; constructing it starts no threads and writes no worlds.
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
    pub fn bind(directory: impl AsRef<Path>, config: ServerConfig) -> io::Result<Self> {
        Self::bind_with_auth(directory, config, Arc::new(NoAuth))
    }
    /// Supply an authenticator. Listening remains loopback-only in this slice.
    pub fn bind_with_auth(
        directory: impl AsRef<Path>,
        config: ServerConfig,
        auth: Arc<dyn Authenticator>,
    ) -> io::Result<Self> {
        if !config.listen.ip().is_loopback() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "no-auth server requires a loopback listen address",
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
        let registry = Arc::new(Registry::open(
            directory.as_ref(),
            config.default_db.clone(),
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
        let mut clients: Vec<Client> = Vec::new();
        let mut next_id = 0u64;
        let result = loop {
            let mut i = 0;
            while i < clients.len() {
                if clients[i].thread.is_finished() {
                    let client = clients.swap_remove(i);
                    let _ = client.thread.join();
                } else {
                    i += 1;
                }
            }
            if shutdown.load(Ordering::Relaxed) {
                break Ok(());
            }
            match self.listener.accept() {
                Ok((stream, _)) => {
                    if clients.len() >= self.config.max_connections {
                        let _ = stream.shutdown(Shutdown::Both);
                        eprintln!("event=bolt_connection_refused reason=connection_cap");
                        continue;
                    }
                    if let Err(e) = stream.set_nodelay(true) {
                        break Err(e);
                    }
                    let socket = match stream.try_clone() {
                        Ok(s) => s,
                        Err(e) => break Err(e),
                    };
                    let registry = Arc::clone(&self.registry);
                    let auth = Arc::clone(&self.auth);
                    let config = self.config.clone();
                    let stop = Arc::clone(&shutdown);
                    next_id = next_id.wrapping_add(1);
                    let id = next_id;
                    let thread =
                        match thread::Builder::new()
                            .name(format!("bolt-{id}"))
                            .spawn(move || {
                                eprintln!("event=bolt_connected connection_id={id}");
                                let result = session::serve(
                                    stream,
                                    &registry,
                                    auth.as_ref(),
                                    &config,
                                    id,
                                    &stop,
                                );
                                eprintln!(
                                    "event=bolt_disconnected connection_id={id} reason={:?}",
                                    result.err().map(|e| e.kind())
                                );
                            }) {
                            Ok(t) => t,
                            Err(e) => break Err(e),
                        };
                    clients.push(Client { socket, thread });
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(5))
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => (),
                Err(e) => break Err(e),
            }
        };
        shutdown.store(true, Ordering::Relaxed);
        for client in &clients {
            let _ = client.socket.shutdown(Shutdown::Both);
        }
        for client in clients {
            let _ = client.thread.join();
        }
        result
    }
}
