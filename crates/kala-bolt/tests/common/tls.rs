//! Each test server gets a fresh key. Its clients trust that server's certificate only.
use kala_bolt::ServerConfig;
use rustls::pki_types::{CertificateDer, pem::PemObject};
use rustls::{ClientConfig, ClientConnection, RootCertStore, StreamOwned};
use std::fs;
use std::io::Write;
use std::net::TcpStream;
use std::path::Path;
use std::sync::Arc;

pub fn configure(dir: &Path, config: &mut ServerConfig) {
    let pair = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
    let cert = dir.join("test-cert.pem");
    let key = dir.join("test-key.pem");
    fs::write(&cert, pair.cert.pem()).unwrap();
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options
        .open(&key)
        .unwrap()
        .write_all(pair.signing_key.serialize_pem().as_bytes())
        .unwrap();
    config.tls_cert = Some(cert);
    config.tls_key = Some(key);
}

pub fn client(dir: &Path) -> ClientConnection {
    client_with_versions(dir, rustls::DEFAULT_VERSIONS)
}

pub fn client_with_versions(
    dir: &Path,
    versions: &[&'static rustls::SupportedProtocolVersion],
) -> ClientConnection {
    let mut roots = RootCertStore::empty();
    roots
        .add(CertificateDer::from_pem_file(dir.join("test-cert.pem")).unwrap())
        .unwrap();
    let config = ClientConfig::builder_with_protocol_versions(versions)
        .with_root_certificates(roots)
        .with_no_client_auth();
    ClientConnection::new(Arc::new(config), "localhost".try_into().unwrap()).unwrap()
}

pub fn stream(dir: &Path, socket: TcpStream) -> StreamOwned<ClientConnection, TcpStream> {
    socket.set_nodelay(true).unwrap();
    StreamOwned::new(client(dir), socket)
}
