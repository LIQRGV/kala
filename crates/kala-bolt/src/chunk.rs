//! Chunk framing and absolute I/O deadlines, including trickling clients.
use crate::transport::Transport;
use std::io;
use std::time::{Duration, Instant};

pub(crate) fn read_exact_until(
    stream: &mut dyn Transport,
    mut bytes: &mut [u8],
    deadline: Instant,
) -> io::Result<()> {
    while !bytes.is_empty() {
        match stream.read_until(bytes, deadline) {
            Ok(0) => return Err(io::ErrorKind::UnexpectedEof.into()),
            Ok(n) => bytes = &mut bytes[n..],
            Err(e) if e.kind() == io::ErrorKind::Interrupted => (),
            Err(e) => return Err(e),
        }
    }
    Ok(())
}
pub(crate) fn write_all_until(
    stream: &mut dyn Transport,
    mut bytes: &[u8],
    deadline: Instant,
) -> io::Result<()> {
    while !bytes.is_empty() {
        match stream.write_until(bytes, deadline) {
            Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
            Ok(n) => bytes = &bytes[n..],
            Err(e) if e.kind() == io::ErrorKind::Interrupted => (),
            Err(e) => return Err(e),
        }
    }
    Ok(())
}
/// Expire transaction state even while a client is idle or trickles a frame.
/// Keep framing progress: the next complete request receives the timeout.
pub(crate) fn read_message_expiring(
    stream: &mut dyn Transport,
    max: usize,
    timeout: Duration,
    expires: Option<Instant>,
    mut expire: impl FnMut(),
) -> io::Result<Vec<u8>> {
    let mut idle = Instant::now() + timeout;
    let mut expires = expires;
    let mut read = |stream: &mut dyn Transport, mut bytes: &mut [u8]| -> io::Result<()> {
        while !bytes.is_empty() {
            if expires.is_some_and(|at| Instant::now() >= at) {
                expire();
                expires = None;
                idle = Instant::now() + timeout;
            }
            let deadline = expires.map_or(idle, |at| at.min(idle));
            match stream.read_until(bytes, deadline) {
                Ok(0) => return Err(io::ErrorKind::UnexpectedEof.into()),
                Ok(n) => bytes = &mut bytes[n..],
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e)
                    if matches!(
                        e.kind(),
                        io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
                    ) && expires.is_some_and(|at| Instant::now() >= at) => {}
                Err(e) => return Err(e),
            }
        }
        Ok(())
    };
    let mut bytes = Vec::new();
    loop {
        let mut header = [0; 2];
        read(stream, &mut header)?;
        let len = usize::from(u16::from_be_bytes(header));
        if len == 0 {
            if bytes.is_empty() {
                continue;
            } // NOOPs do not extend the deadline.
            return Ok(bytes);
        }
        if len > max.saturating_sub(bytes.len()) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Bolt message size limit exceeded",
            ));
        }
        let start = bytes.len();
        bytes.resize(start + len, 0);
        read(stream, &mut bytes[start..])?;
    }
}
pub(crate) fn write_message(
    stream: &mut dyn Transport,
    bytes: &[u8],
    deadline: Instant,
) -> io::Result<()> {
    for chunk in bytes.chunks(u16::MAX as usize) {
        write_all_until(stream, &(chunk.len() as u16).to_be_bytes(), deadline)?;
        write_all_until(stream, chunk, deadline)?;
    }
    write_all_until(stream, &[0; 2], deadline)
}
