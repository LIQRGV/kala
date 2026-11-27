//! A minimal Bolt 5.4 client for tests of the real `kala serve` binary.
#![allow(dead_code)]
use std::io::{Read, Write};

/// One chunked Bolt message.
pub fn send(stream: &mut impl Write, message: &[u8]) {
    stream
        .write_all(&(message.len() as u16).to_be_bytes())
        .unwrap();
    stream.write_all(message).unwrap();
    stream.write_all(&[0, 0]).unwrap();
}

/// Reads one chunked message and requires it to be SUCCESS.
pub fn success(stream: &mut impl Read) {
    let mut message = Vec::new();
    loop {
        let mut header = [0; 2];
        stream.read_exact(&mut header).unwrap();
        let len = usize::from(u16::from_be_bytes(header));
        if len == 0 {
            break;
        }
        let start = message.len();
        message.resize(start + len, 0);
        stream.read_exact(&mut message[start..]).unwrap();
    }
    assert_eq!(message[..2], [0xb1, 0x70], "expected SUCCESS: {message:?}");
}

/// Handshake (Bolt 5.4), HELLO and LOGON with scheme "none".
pub fn log_on(stream: &mut (impl Read + Write)) {
    stream
        .write_all(&[
            0x60, 0x60, 0xb0, 0x17, 0, 0, 4, 5, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        ])
        .unwrap();
    let mut version = [0; 4];
    stream.read_exact(&mut version).unwrap();
    assert_eq!(version, [0, 0, 4, 5]);
    // HELLO {user_agent: "test"}
    send(
        stream,
        &[
            0xb1, 0x01, 0xa1, 0x8a, b'u', b's', b'e', b'r', b'_', b'a', b'g', b'e', b'n', b't',
            0x84, b't', b'e', b's', b't',
        ],
    );
    success(stream);
    // LOGON {scheme: "none"}
    send(
        stream,
        &[
            0xb1, 0x6a, 0xa1, 0x86, b's', b'c', b'h', b'e', b'm', b'e', 0x84, b'n', b'o', b'n',
            b'e',
        ],
    );
    success(stream);
}
