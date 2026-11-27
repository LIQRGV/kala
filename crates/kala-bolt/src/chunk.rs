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
/// Frame one message (chunk headers, bodies, the terminator) and write it as
/// one buffer: one syscall on a socket and, over TLS, records that don't
/// follow chunk boundaries. Chunking is framing, not flow control, so nothing
/// is gained by handing the transport three pieces per message.
pub(crate) fn write_message(
    stream: &mut dyn Transport,
    bytes: &[u8],
    deadline: Instant,
) -> io::Result<()> {
    const CHUNK: usize = u16::MAX as usize;
    let mut framed = Vec::with_capacity(bytes.len() + 2 * bytes.len().div_ceil(CHUNK) + 2);
    for chunk in bytes.chunks(CHUNK) {
        framed.extend_from_slice(&(chunk.len() as u16).to_be_bytes());
        framed.extend_from_slice(chunk);
    }
    framed.extend_from_slice(&[0; 2]);
    write_all_until(stream, &framed, deadline)
}

#[cfg(test)]
mod tests {
    use super::write_message;
    use crate::transport::{ListenAddr, Peer, ShutdownHandle, Transport};
    use std::io;
    use std::time::{Duration, Instant};

    /// Records each write; accepts at most `step` bytes of it.
    struct Sink {
        writes: Vec<Vec<u8>>,
        step: usize,
    }
    impl Transport for Sink {
        fn read_until(&mut self, _: &mut [u8], _: Instant) -> io::Result<usize> {
            unreachable!()
        }
        fn write_until(&mut self, bytes: &[u8], _: Instant) -> io::Result<usize> {
            let n = bytes.len().min(self.step);
            self.writes.push(bytes[..n].to_vec());
            Ok(n)
        }
        fn shutdown_handle(&self) -> io::Result<Box<dyn ShutdownHandle>> {
            unreachable!()
        }
        fn peer(&self) -> io::Result<Peer> {
            unreachable!()
        }
        fn local_addr(&self) -> io::Result<ListenAddr> {
            unreachable!()
        }
    }

    fn written(message: &[u8], step: usize) -> Vec<Vec<u8>> {
        let mut sink = Sink {
            writes: vec![],
            step,
        };
        let deadline = Instant::now() + Duration::from_secs(5);
        write_message(&mut sink, message, deadline).unwrap();
        sink.writes
    }

    /// Chunk headers, bodies and the terminator, as the wire has them.
    fn framed(message: &[u8]) -> Vec<u8> {
        let mut out = vec![];
        for chunk in message.chunks(65_535) {
            out.extend((chunk.len() as u16).to_be_bytes());
            out.extend(chunk);
        }
        out.extend([0, 0]);
        out
    }

    // PR #23 polish: header, body and terminator used to be three writes
    // (three syscalls on TCP, three records over TLS).
    #[test]
    fn a_message_is_framed_into_one_write() {
        let small = [0xb1, 0x70, 0xa0];
        assert_eq!(written(&small, usize::MAX), [framed(&small)]);
        assert_eq!(framed(&small), [0, 3, 0xb1, 0x70, 0xa0, 0, 0]);
        // Exactly one full chunk, one byte more, and several chunks.
        for len in [65_535, 65_536, 200_000] {
            let message: Vec<u8> = (0..len).map(|i| i as u8).collect();
            let writes = written(&message, usize::MAX);
            assert_eq!(writes.len(), 1, "{len}");
            assert_eq!(writes[0], framed(&message), "{len}");
        }
        assert_eq!(framed(&[7; 65_536]).len(), 65_536 + 2 + 2 + 2);
    }

    #[test]
    fn partial_writes_still_deliver_the_whole_frame_in_order() {
        let message: Vec<u8> = (0..70_000).map(|i| (i * 7) as u8).collect();
        let writes = written(&message, 4096);
        assert!(writes.len() > 17);
        assert_eq!(writes.concat(), framed(&message));
    }
}
