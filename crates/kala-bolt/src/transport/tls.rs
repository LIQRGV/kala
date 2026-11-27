//! Blocking TLS on a deadline-aware byte transport.
use super::{ListenAddr, Peer, ShutdownHandle, Transport, remaining};
use crate::log::logln;
use rustls::{ServerConfig, ServerConnection};
use std::io::{self, Read, Write};
use std::sync::Arc;
use std::time::Instant;

mod config;
mod der;
pub use config::TlsReloader;

pub(super) struct Tls {
    inner: Box<dyn Transport>,
    conn: ServerConnection,
}

/// rustls pulls/pushes ciphertext through std I/O. Every partial operation
/// and every interrupted syscall shares the caller's original deadline.
struct RecordIo<'a> {
    inner: &'a mut dyn Transport,
    deadline: Instant,
}

impl Read for RecordIo<'_> {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        loop {
            remaining(self.deadline)?;
            match self.inner.read_until(bytes, self.deadline) {
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                result => return result,
            }
        }
    }
}

impl Write for RecordIo<'_> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        loop {
            remaining(self.deadline)?;
            match self.inner.write_until(bytes, self.deadline) {
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                result => return result,
            }
        }
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Tls {
    pub(super) fn new(inner: Box<dyn Transport>, config: Arc<ServerConfig>) -> io::Result<Self> {
        let mut conn = ServerConnection::new(config).map_err(io::Error::other)?;
        conn.set_buffer_limit(Some(64 * 1024));
        Ok(Self { inner, conn })
    }

    fn send_records(&mut self, deadline: Instant) -> io::Result<()> {
        let mut writer = RecordIo {
            inner: self.inner.as_mut(),
            deadline,
        };
        while self.conn.wants_write() {
            if self.conn.write_tls(&mut writer)? == 0 {
                return Err(io::ErrorKind::WriteZero.into());
            }
        }
        Ok(())
    }

    fn receive_record(&mut self, deadline: Instant) -> io::Result<()> {
        let mut reader = RecordIo {
            inner: self.inner.as_mut(),
            deadline,
        };
        if self.conn.read_tls(&mut reader)? == 0 {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        self.conn.process_new_packets().map_err(|e| {
            // Only static protocol classes may reach logs, never SNI or peer data.
            use rustls::Error::*;
            let class = match e {
                InvalidMessage(_) => "invalid_message",
                PeerIncompatible(_) => "peer_incompatible",
                PeerMisbehaved(_) => "peer_misbehaved",
                AlertReceived(_) => "alert_received",
                PeerSentOversizedRecord => "oversized_record",
                DecryptError => "decrypt_error",
                InappropriateMessage { .. } | InappropriateHandshakeMessage { .. } => {
                    "unexpected_message"
                }
                _ => "tls_protocol_error",
            };
            io::Error::new(io::ErrorKind::ConnectionAborted, TlsClass(class))
        })?;
        Ok(())
    }

    fn handshake(&mut self, deadline: Instant) -> io::Result<()> {
        if !self.conn.is_handshaking() {
            return Ok(());
        }
        let result = (|| {
            while self.conn.is_handshaking() {
                self.send_records(deadline)?;
                self.receive_record(deadline)?;
            }
            self.send_records(deadline)
        })();
        if let Err(error) = &result {
            // Underlying I/O errors can contain arbitrary strings. Only our
            // own rustls protocol classes and the I/O kind are safe to log.
            logln!(
                "event=bolt_connection_dropped reason=tls_handshake error={:?} class={}",
                error.kind(),
                error
                    .get_ref()
                    .and_then(|e| e.downcast_ref::<TlsClass>())
                    .map_or("io", |c| c.0)
            );
        }
        result
    }
}

#[derive(Debug)]
struct TlsClass(&'static str);
impl std::fmt::Display for TlsClass {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}
impl std::error::Error for TlsClass {}

impl Transport for Tls {
    fn read_until(&mut self, bytes: &mut [u8], deadline: Instant) -> io::Result<usize> {
        remaining(deadline)?;
        self.handshake(deadline)?;
        loop {
            match self.conn.reader().read(bytes) {
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    self.receive_record(deadline)?;
                    self.send_records(deadline)?;
                }
                result => return result,
            }
        }
    }
    fn write_until(&mut self, bytes: &[u8], deadline: Instant) -> io::Result<usize> {
        remaining(deadline)?;
        self.handshake(deadline)?;
        let n = self.conn.writer().write(bytes)?;
        self.send_records(deadline)?;
        Ok(n)
    }
    fn finish(&mut self, deadline: Instant) -> io::Result<()> {
        if self.conn.is_handshaking() {
            return Ok(());
        }
        self.conn.send_close_notify();
        self.send_records(deadline)?;
        self.inner.finish(deadline)
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

#[cfg(test)]
mod tests;
