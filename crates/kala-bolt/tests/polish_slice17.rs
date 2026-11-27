//! PR #23 (slice 17) review polish, against the real server over a socket.
#[path = "common/client.rs"]
mod client;
mod common;
#[path = "common/tls.rs"]
mod tls;

use client::{Client, map, string};
use common::Running;
use kala_bolt::transport::{ListenAddr, Peer, ShutdownHandle, Transport};
use kala_bolt::{Server, ServerConfig};
use std::io::{self, Read, Write};
use std::sync::{Arc, Mutex};
use std::time::Instant;

/// Records the size of every write the server makes below its framing (and
/// below TLS, on a TLS listener).
struct Counting {
    inner: Box<dyn Transport>,
    writes: Arc<Mutex<Vec<usize>>>,
}
impl Transport for Counting {
    fn read_until(&mut self, bytes: &mut [u8], deadline: Instant) -> io::Result<usize> {
        self.inner.read_until(bytes, deadline)
    }
    fn write_until(&mut self, bytes: &[u8], deadline: Instant) -> io::Result<usize> {
        let n = self.inner.write_until(bytes, deadline)?;
        self.writes.lock().unwrap().push(n);
        Ok(n)
    }
    fn finish(&mut self, deadline: Instant) -> io::Result<()> {
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

fn counting_server(listen: &str) -> (Running, Arc<Mutex<Vec<usize>>>) {
    let dir = tempfile::tempdir().unwrap();
    let mut config = ServerConfig {
        listen: vec![listen.parse().unwrap()],
        ..ServerConfig::default()
    };
    if listen.starts_with("bolt+s") {
        tls::configure(dir.path(), &mut config);
    }
    let writes = Arc::new(Mutex::new(vec![]));
    let record = writes.clone();
    let server = Server::bind_accounts(dir.path(), config)
        .unwrap()
        .with_transport_wrapper(move |inner| {
            Box::new(Counting {
                inner,
                writes: record.clone(),
            })
        });
    (Running::from_server(dir, server), writes)
}

fn logged_on<S: Read + Write>(stream: S) -> Client<S> {
    let mut c = Client { stream };
    common::handshake(&mut c.stream, 4);
    c.hello();
    c.success();
    c.send(0x6a, vec![map(&[("scheme", string("none"))])]);
    c.success();
    c
}

// A Bolt message used to cost three writes (chunk header, body, terminator):
// three syscalls on TCP and three TLS records, which also showed chunk
// boundaries on the wire. It is one write on every transport.
#[test]
fn one_small_response_is_one_write_below_tls_and_on_plain_tcp() {
    // SUCCESS {} framed: 2 + 3 + 2 bytes; a TLS 1.3 record adds 5 bytes of
    // header, 1 of content type and a 16-byte tag.
    for (listen, size) in [("bolt+s://127.0.0.1:0", 7 + 22), ("127.0.0.1:0", 7)] {
        let (server, writes) = counting_server(listen);
        let after_logon = |writes: &Mutex<Vec<usize>>| writes.lock().unwrap().len();
        if listen.starts_with("bolt+s") {
            let mut c = logged_on(tls::stream(server.dir.path(), server.connect()));
            let before = after_logon(&writes);
            c.send(0x0f, vec![]);
            c.success();
            assert_eq!(writes.lock().unwrap()[before..], [size], "{listen}");
        } else {
            let mut c = logged_on(server.connect());
            let before = after_logon(&writes);
            c.send(0x0f, vec![]);
            c.success();
            assert_eq!(writes.lock().unwrap()[before..], [size], "{listen}");
        }
    }
}
