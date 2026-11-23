//! Chunk framing and absolute I/O deadlines, including trickling clients.
use std::io::{self, Read, Write};
use std::net::TcpStream;
use std::time::{Duration, Instant};

fn remaining(deadline: Instant) -> io::Result<Duration> {
    deadline
        .checked_duration_since(Instant::now())
        .filter(|d| !d.is_zero())
        .ok_or_else(|| io::Error::new(io::ErrorKind::TimedOut, "Bolt I/O deadline exceeded"))
}
pub(crate) fn read_exact_until(
    stream: &mut TcpStream,
    mut bytes: &mut [u8],
    deadline: Instant,
) -> io::Result<()> {
    while !bytes.is_empty() {
        stream.set_read_timeout(Some(remaining(deadline)?))?;
        match stream.read(bytes) {
            Ok(0) => return Err(io::ErrorKind::UnexpectedEof.into()),
            Ok(n) => bytes = &mut bytes[n..],
            Err(e) if e.kind() == io::ErrorKind::Interrupted => (),
            Err(e) => return Err(e),
        }
    }
    Ok(())
}
fn write_all_until(stream: &mut TcpStream, mut bytes: &[u8], deadline: Instant) -> io::Result<()> {
    while !bytes.is_empty() {
        stream.set_write_timeout(Some(remaining(deadline)?))?;
        match stream.write(bytes) {
            Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
            Ok(n) => bytes = &bytes[n..],
            Err(e) if e.kind() == io::ErrorKind::Interrupted => (),
            Err(e) => return Err(e),
        }
    }
    Ok(())
}
pub(crate) fn read_message(
    stream: &mut TcpStream,
    max: usize,
    timeout: Duration,
) -> io::Result<Vec<u8>> {
    let deadline = Instant::now() + timeout;
    let mut bytes = Vec::new();
    loop {
        let mut header = [0; 2];
        read_exact_until(stream, &mut header, deadline)?;
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
        read_exact_until(stream, &mut bytes[start..], deadline)?;
    }
}
pub(crate) fn write_message(
    stream: &mut TcpStream,
    bytes: &[u8],
    deadline: Instant,
) -> io::Result<()> {
    for chunk in bytes.chunks(u16::MAX as usize) {
        write_all_until(stream, &(chunk.len() as u16).to_be_bytes(), deadline)?;
        write_all_until(stream, chunk, deadline)?;
    }
    write_all_until(stream, &[0; 2], deadline)
}
