//! One validated identity shared by all TLS listeners. Replacements are built
//! off-lock, so reading the files never blocks handshakes on the old identity.
use super::super::{ListenAddr, invalid};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, pem::PemObject};
use rustls::{ServerConfig, crypto::ring, server::NoServerSessionStorage};
use std::fs::{File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use x509_parser::prelude::{ASN1Time, parse_x509_certificate};

/// Reload the PEM files configured at startup. Each accepted TLS connection
/// retains a snapshot of the identity in force when it was admitted.
#[derive(Clone)]
pub struct TlsReloader(Arc<Identity>);

struct Identity {
    cert: PathBuf,
    key: PathBuf,
    current: RwLock<Arc<ServerConfig>>,
}

impl TlsReloader {
    pub(crate) fn configured(
        addrs: &[ListenAddr],
        cert: Option<&Path>,
        key: Option<&Path>,
    ) -> io::Result<Option<Self>> {
        let tls = addrs.iter().any(|addr| matches!(addr, ListenAddr::Tls(_)));
        match (tls, cert, key) {
            (true, Some(cert), Some(key)) => Ok(Some(Self(Arc::new(Identity {
                current: RwLock::new(Arc::new(load(cert, key)?)),
                cert: cert.into(),
                key: key.into(),
            })))),
            (true, _, _) => Err(invalid(
                "a bolt+s:// listener requires both --tls.cert and --tls.key",
            )),
            (false, None, None) => Ok(None),
            (false, _, _) => Err(invalid(
                "--tls.cert and --tls.key require a bolt+s:// listener",
            )),
        }
    }

    /// Re-read and validate both files. A failed reload preserves the active pair.
    pub fn reload(&self) -> io::Result<()> {
        let next = Arc::new(load(&self.0.cert, &self.0.key)?);
        *self.0.current.write().unwrap_or_else(|p| p.into_inner()) = next;
        Ok(())
    }

    pub(in crate::transport) fn current(&self) -> Arc<ServerConfig> {
        self.0
            .current
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }
}

fn open_file(path: &Path, private: bool) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        // A FIFO mistakenly installed during rotation must not block sigwait.
        options.custom_flags(libc::O_NONBLOCK);
    }
    let file = options
        .open(path)
        .map_err(|e| io::Error::new(e.kind(), format!("opening TLS file {path:?}: {e}")))?;
    let meta = file
        .metadata()
        .map_err(|e| io::Error::new(e.kind(), format!("checking TLS file {path:?}: {e}")))?;
    if !meta.is_file() {
        return Err(invalid(format!("TLS file {path:?} must be a regular file")));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if private && meta.permissions().mode() & 0o044 != 0 {
            return Err(invalid(format!(
                "insecure private key permissions on {path:?}: remove group and other read access (chmod 600)"
            )));
        }
    }
    #[cfg(not(unix))]
    let _ = private;
    Ok(file)
}

fn load(cert: &Path, key: &Path) -> io::Result<ServerConfig> {
    let chain = CertificateDer::pem_reader_iter(open_file(cert, false)?)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| invalid(format!("invalid PEM certificate chain in {cert:?}")))?;
    if chain.is_empty() {
        return Err(invalid(format!("no certificates in {cert:?}")));
    }
    let now = ASN1Time::now().timestamp();
    let mut expiry_warning = None;
    for (i, bytes) in chain.iter().enumerate() {
        let (_, parsed) = parse_x509_certificate(bytes)
            .ok()
            .filter(|(rest, _)| rest.is_empty())
            .ok_or_else(|| invalid(format!("invalid certificate {} in {cert:?}", i + 1)))?;
        if i == 0 {
            let start = parsed.validity().not_before;
            let end = parsed.validity().not_after;
            let problem = if now < start.timestamp() {
                Some("not yet valid")
            } else if now > end.timestamp() {
                Some("expired")
            } else {
                None
            };
            if let Some(reason) = problem {
                return Err(invalid(format!(
                    "leaf certificate in {cert:?} is {reason} (valid from {} to {})",
                    start.to_datetime(),
                    end.to_datetime()
                )));
            }
            if end.timestamp() - now <= 14 * 86400 {
                expiry_warning = Some(end.to_datetime());
            }
        }
    }
    let private = PrivateKeyDer::from_pem_reader(open_file(key, true)?).map_err(|_| {
        invalid(format!(
            "expected an unencrypted PEM private key in {key:?}"
        ))
    })?;
    let mut config = ServerConfig::builder_with_provider(Arc::new(ring::default_provider()))
        .with_safe_default_protocol_versions()
        .map_err(io::Error::other)?
        .with_no_client_auth()
        .with_single_cert(chain, private)
        .map_err(|e| match e {
            rustls::Error::InconsistentKeys(_) => invalid(format!(
                "private key {key:?} does not match leaf certificate in {cert:?}"
            )),
            _ => invalid(format!(
                "invalid private key {key:?} or leaf certificate in {cert:?}"
            )),
        })?;
    config.session_storage = Arc::new(NoServerSessionStorage {});
    config.send_tls13_tickets = 0;
    if let Some(expiry) = expiry_warning {
        eprintln!("kala serve: WARNING: leaf certificate in {cert:?} expires on {expiry}");
    }
    Ok(config)
}
