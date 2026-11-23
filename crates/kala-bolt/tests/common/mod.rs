#![allow(dead_code)]

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use kala_bolt::{Server, ServerConfig};

pub const VARROW: &str = include_str!("../../../kala-core/tests/fixtures/varrow.yaml");

pub struct Running {
    pub dir: tempfile::TempDir,
    pub addr: SocketAddr,
    pub shutdown: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Running {
    pub fn start() -> Self {
        Self::with_config(ServerConfig::default())
    }

    pub fn with_config(mut config: ServerConfig) -> Self {
        config.listen = "127.0.0.1:0".parse().unwrap();
        let dir = tempfile::tempdir().unwrap();
        drop(kala_core::load_fixture_to(VARROW, &dir.path().join("varrow.kala")).unwrap());
        Self::in_dir(dir, config)
    }

    pub fn in_dir(dir: tempfile::TempDir, config: ServerConfig) -> Self {
        let server = Server::bind(dir.path(), config).unwrap();
        Self::from_server(dir, server)
    }

    pub fn with_auth(auth: Arc<dyn kala_bolt::auth::Authenticator>) -> Self {
        let dir = tempfile::tempdir().unwrap();
        drop(kala_core::load_fixture_to(VARROW, &dir.path().join("varrow.kala")).unwrap());
        let config = ServerConfig {
            listen: "127.0.0.1:0".parse().unwrap(),
            ..ServerConfig::default()
        };
        let server = Server::bind_with_auth(dir.path(), config, auth).unwrap();
        Self::from_server(dir, server)
    }

    fn from_server(dir: tempfile::TempDir, server: Server) -> Self {
        let addr = server.local_addr().unwrap();
        let shutdown = Arc::new(AtomicBool::new(false));
        let stop = shutdown.clone();
        let thread = thread::spawn(move || server.run(stop).unwrap());
        Self {
            dir,
            addr,
            shutdown,
            thread: Some(thread),
        }
    }

    pub fn connect(&self) -> TcpStream {
        let stream = TcpStream::connect(self.addr).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        stream
            .set_write_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        stream
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::SeqCst);
        // Wake a blocking accept, if the implementation uses one.
        let _ = TcpStream::connect(self.addr);
        if let Some(thread) = self.thread.take() {
            thread.join().unwrap();
        }
    }
}

pub fn handshake(stream: &mut TcpStream, minor: u8) {
    let mut bytes = [0_u8; 20];
    bytes[..4].copy_from_slice(&[0x60, 0x60, 0xb0, 0x17]);
    bytes[6] = minor;
    bytes[7] = 5;
    stream.write_all(&bytes).unwrap();
    let mut response = [0; 4];
    stream.read_exact(&mut response).unwrap();
    assert_eq!(response, [0, 0, minor, 5]);
}

pub fn assert_closed(stream: &mut TcpStream) {
    let mut byte = [0];
    match stream.read(&mut byte) {
        Ok(0) => (),
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::ConnectionReset
                    | std::io::ErrorKind::ConnectionAborted
                    | std::io::ErrorKind::BrokenPipe
            ) => {}
        other => panic!("expected the server to close the connection, got {other:?}"),
    }
}
