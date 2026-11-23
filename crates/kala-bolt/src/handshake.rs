//! Bolt 5.0–5.4 version negotiation.
use crate::chunk::read_exact_until;
use std::io::{self, Write};
use std::net::TcpStream;
use std::time::{Duration, Instant};

pub(crate) fn negotiate(stream: &mut TcpStream, timeout: Duration) -> io::Result<Option<u8>> {
    let mut bytes = [0; 20];
    read_exact_until(stream, &mut bytes, Instant::now() + timeout)?;
    if bytes[..4] != [0x60, 0x60, 0xb0, 0x17] {
        return Ok(None);
    }
    let minor = bytes[4..]
        .as_chunks::<4>()
        .0
        .iter()
        .filter_map(|p| {
            if p[0] != 0 || p[3] != 5 {
                return None;
            }
            let high = p[2].min(4);
            (high >= p[2].saturating_sub(p[1])).then_some(high)
        })
        .max();
    stream.write_all(&minor.map_or([0; 4], |m| [0, 0, m, 5]))?;
    Ok(minor)
}
