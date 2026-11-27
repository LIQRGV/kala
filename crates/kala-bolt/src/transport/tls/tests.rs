//! Exercise record fragmentation without relying on OS packet boundaries.
use super::*;
use rustls::{
    ClientConfig, ClientConnection, RootCertStore, ServerConfig, pki_types::PrivatePkcs8KeyDer,
};
use std::io::{Read, Write};
use std::sync::Arc;
use std::time::Duration;

struct Fragments {
    incoming: io::Cursor<Vec<u8>>,
    deadline: Instant,
    stall: io::ErrorKind,
}

impl Transport for Fragments {
    fn read_until(&mut self, bytes: &mut [u8], deadline: Instant) -> io::Result<usize> {
        assert_eq!(deadline, self.deadline);
        match self.incoming.read(&mut bytes[..1])? {
            0 => Err(self.stall.into()),
            n => Ok(n),
        }
    }
    fn write_until(&mut self, _: &[u8], deadline: Instant) -> io::Result<usize> {
        assert_eq!(deadline, self.deadline);
        Err(self.stall.into())
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

fn connections() -> (ServerConnection, ClientConnection) {
    let pair = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
    let cert = pair.cert.der().clone();
    let mut roots = RootCertStore::empty();
    roots.add(cert.clone()).unwrap();
    let server = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(
            vec![cert],
            PrivatePkcs8KeyDer::from(pair.signing_key.serialize_der()).into(),
        )
        .unwrap();
    let client = ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    (
        ServerConnection::new(Arc::new(server)).unwrap(),
        ClientConnection::new(Arc::new(client), "localhost".try_into().unwrap()).unwrap(),
    )
}

fn pipe_tls(client: &mut ClientConnection, server: &mut ServerConnection) {
    while client.is_handshaking() || server.is_handshaking() {
        let mut outbound = vec![];
        while client.wants_write() {
            client.write_tls(&mut outbound).unwrap();
        }
        if !outbound.is_empty() {
            server.read_tls(&mut outbound.as_slice()).unwrap();
            server.process_new_packets().unwrap();
        }
        let mut inbound = vec![];
        while server.wants_write() {
            server.write_tls(&mut inbound).unwrap();
        }
        if !inbound.is_empty() {
            client.read_tls(&mut inbound.as_slice()).unwrap();
            client.process_new_packets().unwrap();
        }
    }
}

fn fragmented(
    conn: ServerConnection,
    incoming: Vec<u8>,
    stall: io::ErrorKind,
    deadline: Instant,
) -> Tls {
    Tls {
        conn,
        inner: Box::new(Fragments {
            incoming: io::Cursor::new(incoming),
            deadline,
            stall,
        }),
    }
}

#[test]
fn partial_client_hello_preserves_read_deadline() {
    let (server, mut client) = connections();
    let mut hello = vec![];
    client.write_tls(&mut hello).unwrap();
    hello.truncate(8);
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut tls = fragmented(server, hello, io::ErrorKind::TimedOut, deadline);
    assert_eq!(
        tls.read_until(&mut [0], deadline).unwrap_err().kind(),
        io::ErrorKind::TimedOut
    );
}

#[test]
fn partial_application_record_preserves_read_deadline() {
    let (mut server, mut client) = connections();
    pipe_tls(&mut client, &mut server);
    client.writer().write_all(b"fragmented").unwrap();
    let mut record = vec![];
    client.write_tls(&mut record).unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut partial = record.clone();
    partial.pop();
    let mut tls = fragmented(server, partial, io::ErrorKind::WouldBlock, deadline);
    assert_eq!(
        tls.read_until(&mut [0; 16], deadline).unwrap_err().kind(),
        io::ErrorKind::WouldBlock
    );
    tls.inner = Box::new(Fragments {
        incoming: io::Cursor::new(record.split_off(record.len() - 1)),
        deadline,
        stall: io::ErrorKind::WouldBlock,
    });
    let mut plaintext = [0; 10];
    assert_eq!(
        tls.read_until(&mut plaintext, deadline).unwrap(),
        plaintext.len()
    );
    assert_eq!(&plaintext, b"fragmented");
}
