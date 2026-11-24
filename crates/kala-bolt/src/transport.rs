//! Byte transports for one shared Bolt session. Wrappers retain the absolute
//! deadline and delegate cancellation to the underlying connection.
pub use kala_system::auth::Peer;
use std::io::{self, Read, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::time::Instant;

/// Cancellation handle owned by the accept loop, independent of blocked I/O.
pub trait ShutdownHandle: Send {
    /// Interrupt reads and writes in both directions.
    fn shutdown(&self) -> io::Result<()>;
}

/// A blocking byte stream with absolute deadlines, independent cancellation,
/// and connection metadata. TLS and framing adapters can wrap this interface
/// without exposing an OS descriptor to the protocol implementation.
pub trait Transport: Send {
    /// Read some bytes before the deadline; zero denotes EOF.
    fn read_until(&mut self, bytes: &mut [u8], deadline: Instant) -> io::Result<usize>;
    /// Write some bytes before the deadline; partial writes are supported.
    fn write_until(&mut self, bytes: &[u8], deadline: Instant) -> io::Result<usize>;
    /// Obtain an independent handle that interrupts blocked I/O.
    fn shutdown_handle(&self) -> io::Result<Box<dyn ShutdownHandle>>;
    /// The peer identity used for account matching.
    fn peer(&self) -> io::Result<Peer>;
    /// The reached endpoint, used when no advertised address is configured.
    fn local_addr(&self) -> io::Result<ListenAddr>;
}

fn remaining(deadline: Instant) -> io::Result<std::time::Duration> {
    deadline
        .checked_duration_since(Instant::now())
        .filter(|d| !d.is_zero())
        .ok_or_else(|| io::Error::new(io::ErrorKind::TimedOut, "Bolt I/O deadline exceeded"))
}

struct Tcp(TcpStream);
impl ShutdownHandle for TcpStream {
    fn shutdown(&self) -> io::Result<()> {
        self.shutdown(Shutdown::Both)
    }
}
impl Transport for Tcp {
    fn read_until(&mut self, bytes: &mut [u8], deadline: Instant) -> io::Result<usize> {
        self.0.set_read_timeout(Some(remaining(deadline)?))?;
        self.0.read(bytes)
    }
    fn write_until(&mut self, bytes: &[u8], deadline: Instant) -> io::Result<usize> {
        self.0.set_write_timeout(Some(remaining(deadline)?))?;
        self.0.write(bytes)
    }
    fn shutdown_handle(&self) -> io::Result<Box<dyn ShutdownHandle>> {
        Ok(Box::new(self.0.try_clone()?))
    }
    fn peer(&self) -> io::Result<Peer> {
        Ok(Peer::Ip(self.0.peer_addr()?.ip()))
    }
    fn local_addr(&self) -> io::Result<ListenAddr> {
        Ok(ListenAddr::Tcp(self.0.local_addr()?))
    }
}

pub(crate) enum Listener {
    Tcp(TcpListener),
    #[cfg(unix)]
    Unix(unix::BoundUnix),
}
impl Listener {
    pub(crate) fn bind(addr: &ListenAddr, mode: u32) -> io::Result<Self> {
        match addr {
            ListenAddr::Tcp(addr) => {
                let listener = TcpListener::bind(addr)?;
                listener.set_nonblocking(true)?;
                Ok(Self::Tcp(listener))
            }
            ListenAddr::Unix(path) => {
                #[cfg(unix)]
                {
                    unix::BoundUnix::bind(path, mode).map(Self::Unix)
                }
                #[cfg(not(unix))]
                {
                    let _ = (path, mode);
                    Err(invalid("unix:// is not supported on this platform"))
                }
            }
        }
    }
    pub(crate) fn local_addr(&self) -> io::Result<ListenAddr> {
        match self {
            Self::Tcp(listener) => Ok(ListenAddr::Tcp(listener.local_addr()?)),
            #[cfg(unix)]
            Self::Unix(listener) => Ok(ListenAddr::Unix(listener.path.clone())),
        }
    }
    /// Take one pending connection, untouched. An error here is about the
    /// listener (or the accept call), never about the accepted socket: its
    /// setup is `Accepted::into_transport`, which fails per connection.
    pub(crate) fn accept(&self) -> io::Result<Accepted> {
        match self {
            Self::Tcp(listener) => Ok(Accepted::Tcp(listener.accept()?.0)),
            #[cfg(unix)]
            Self::Unix(listener) => listener.accept(),
        }
    }
}

/// A connection as `accept` returned it, before any per-socket setup.
pub(crate) enum Accepted {
    Tcp(TcpStream),
    #[cfg(unix)]
    Unix(unix::AcceptedUnix),
}
impl Accepted {
    /// Per-socket setup. A client that reset right after connecting makes
    /// these calls fail (EINVAL on macOS), which must cost only that client.
    pub(crate) fn into_transport(self) -> io::Result<Box<dyn Transport>> {
        match self {
            Self::Tcp(stream) => {
                // Some platforms inherit nonblocking mode from the listener.
                stream.set_nonblocking(false)?;
                stream.set_nodelay(true)?;
                Ok(Box::new(Tcp(stream)))
            }
            #[cfg(unix)]
            Self::Unix(accepted) => accepted.into_transport(),
        }
    }
    /// Close a connection that will not be served (over the connection cap).
    pub(crate) fn close(self) {
        let _ = match self {
            Self::Tcp(stream) => stream.shutdown(Shutdown::Both),
            #[cfg(unix)]
            Self::Unix(accepted) => accepted.close(),
        };
    }
}

#[cfg(unix)]
mod unix;

/// A configured or bound Bolt endpoint.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum ListenAddr {
    /// Bolt over TCP.
    Tcp(SocketAddr),
    /// Bolt over a filesystem Unix socket.
    Unix(std::path::PathBuf),
}
impl std::fmt::Display for ListenAddr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Tcp(a) => a.fmt(f),
            Self::Unix(p) => write!(f, "unix://{}", p.display()),
        }
    }
}
impl ListenAddr {
    /// Whether the listener is restricted to the local machine.
    pub fn is_loopback(&self) -> bool {
        match self {
            Self::Tcp(a) => a.ip().is_loopback(),
            Self::Unix(_) => true,
        }
    }
}
impl std::str::FromStr for ListenAddr {
    type Err = io::Error;
    fn from_str(value: &str) -> io::Result<Self> {
        let value = match value.split_once("://") {
            Some(("bolt", address)) => address,
            Some(("unix", path)) => {
                let addr = Self::Unix(path.into());
                addr.validate()?;
                return Ok(addr);
            }
            Some(("neo4j", _)) => return Err(invalid("neo4j:// is not a listen address")),
            Some((scheme, _)) => return Err(invalid(format!("{scheme}:// is not supported yet"))),
            None => value,
        };
        let addr = if let Some(port) = value.strip_prefix("localhost:") {
            SocketAddr::from((
                [127, 0, 0, 1],
                port.parse::<u16>()
                    .map_err(|_| invalid("invalid listen port"))?,
            ))
        } else {
            value.parse().map_err(|_| {
                invalid("listen address must be IP:PORT, [IPv6]:PORT or localhost:PORT (no DNS)")
            })?
        };
        Ok(Self::Tcp(addr))
    }
}

fn invalid(message: impl Into<Box<dyn std::error::Error + Send + Sync>>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}
impl ListenAddr {
    pub(crate) fn validate(&self) -> io::Result<()> {
        if let Self::Unix(path) = self {
            #[cfg(unix)]
            {
                unix::validate_path(path)?;
            }
            #[cfg(not(unix))]
            {
                let _ = path;
                return Err(invalid("unix:// is not supported on this platform"));
            }
        }
        Ok(())
    }
}
