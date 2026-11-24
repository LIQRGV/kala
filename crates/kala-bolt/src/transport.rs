//! Byte transports for one shared Bolt session. Wrappers retain the absolute
//! deadline and delegate cancellation to the underlying connection.
use std::io::{self, Read, Write};
use std::net::{IpAddr, Shutdown, SocketAddr, TcpListener, TcpStream};
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
    fn peer(&self) -> io::Result<IpAddr>;
    /// The reached endpoint, used when no advertised address is configured.
    fn local_addr(&self) -> io::Result<SocketAddr>;
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
    fn peer(&self) -> io::Result<IpAddr> {
        Ok(self.0.peer_addr()?.ip())
    }
    fn local_addr(&self) -> io::Result<SocketAddr> {
        self.0.local_addr()
    }
}

pub(crate) struct Listener(TcpListener);
impl Listener {
    pub(crate) fn bind(addr: SocketAddr) -> io::Result<Self> {
        let listener = TcpListener::bind(addr)?;
        listener.set_nonblocking(true)?;
        Ok(Self(listener))
    }
    pub(crate) fn local_addr(&self) -> io::Result<SocketAddr> {
        self.0.local_addr()
    }
    pub(crate) fn accept(&self) -> io::Result<Box<dyn Transport>> {
        let (stream, _) = self.0.accept()?;
        // Some platforms inherit nonblocking mode from the listener.
        stream.set_nonblocking(false)?;
        stream.set_nodelay(true)?;
        Ok(Box::new(Tcp(stream)))
    }
}
