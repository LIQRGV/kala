//! One validated identity shared by all TLS listeners. Replacements are built
//! off-lock, so reading the files never blocks handshakes on the old identity.
use super::super::{ListenAddr, invalid};
use super::der;
use crate::log::logln;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, pem::PemObject};
use rustls::{ServerConfig, crypto::ring, server::NoServerSessionStorage};
use std::fs::{File, OpenOptions};
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

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

const MAX_PEM_BYTES: u64 = 1 << 20;

fn read_pem<T>(
    path: &Path,
    private: bool,
    parse: impl FnOnce(&mut io::Take<File>) -> io::Result<T>,
) -> io::Result<T> {
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
        let mode = meta.permissions().mode() & 0o7777;
        // As ssh does: no access at all for group and others. A key others
        // can write is as lost as one they can read.
        if private && mode & 0o077 != 0 {
            return Err(invalid(format!(
                "insecure private key permissions on {path:?}: mode {mode:04o} gives group or others access; make it readable by its owner only (chmod 600)"
            )));
        }
        // The chain is public, but whoever can write it chooses what the
        // next reload serves.
        if !private && mode & 0o022 != 0 {
            logln!(
                "kala serve: WARNING: certificate file {path:?} is writable by group or others (mode {mode:04o})"
            );
        }
    }
    #[cfg(not(unix))]
    let _ = private;
    let too_large = || invalid(format!("TLS file {path:?} exceeds the 1 MiB limit"));
    if meta.len() > MAX_PEM_BYTES {
        return Err(too_large());
    }
    // Also bound the decoder if the file grows after the descriptor check.
    let mut reader = file.take(MAX_PEM_BYTES + 1);
    let parsed = parse(&mut reader);
    if reader.limit() == 0 {
        return Err(too_large());
    }
    parsed
}

/// How close to its end a leaf's validity gets a warning.
const EXPIRY_WARNING_SECONDS: i64 = 14 * 86400;

/// Every certificate has the X.509 outer shape and the leaf is valid at
/// `now` (Unix seconds). Returns the leaf's end date when it is within 14
/// days. The leaf's contents are parsed again, fully, by rustls.
fn check_chain(
    chain: &[CertificateDer<'_>],
    cert: &Path,
    now: i64,
) -> io::Result<Option<der::Time>> {
    let unparsed = |i: usize| invalid(format!("invalid certificate {} in {cert:?}", i + 1));
    let mut expiry_warning = None;
    for (i, bytes) in chain.iter().enumerate() {
        if i > 0 {
            if !der::is_certificate(bytes) {
                return Err(unparsed(i));
            }
            continue;
        }
        let (start, end) = der::validity(bytes).ok_or_else(|| unparsed(i))?;
        let problem = if now < start.unix() {
            Some("not yet valid")
        } else if now > end.unix() {
            Some("expired")
        } else {
            None
        };
        if let Some(reason) = problem {
            return Err(invalid(format!(
                "leaf certificate in {cert:?} is {reason} (valid from {start} to {end})"
            )));
        }
        if end.unix() - now <= EXPIRY_WARNING_SECONDS {
            expiry_warning = Some(end);
        }
    }
    Ok(expiry_warning)
}

fn load(cert: &Path, key: &Path) -> io::Result<ServerConfig> {
    let chain = read_pem(cert, false, |reader| {
        CertificateDer::pem_reader_iter(reader)
            .collect::<Result<Vec<_>, _>>()
            .map_err(|_| invalid(format!("invalid PEM certificate chain in {cert:?}")))
    })?;
    if chain.is_empty() {
        return Err(invalid(format!("no certificates in {cert:?}")));
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| {
            i64::try_from(since.as_secs()).unwrap_or(i64::MAX)
        });
    let expiry_warning = check_chain(&chain, cert, now)?;
    let private = read_pem(key, true, |reader| {
        PrivateKeyDer::from_pem_reader(reader).map_err(|_| {
            invalid(format!(
                "expected an unencrypted PEM private key in {key:?}"
            ))
        })
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
        logln!("kala serve: WARNING: leaf certificate in {cert:?} expires on {expiry}");
    }
    Ok(config)
}

#[cfg(test)]
mod tests {
    use super::{CertificateDer, check_chain};
    use rcgen::{CertificateParams, KeyPair, date_time_ymd};
    use std::path::Path;

    const DAY: i64 = 86400;

    /// A certificate valid from 2030-01-01 to 2031-01-01 (UTCTime) or from
    /// 2060-01-01 to 2061-01-01 (GeneralizedTime), and those instants.
    fn leaf(year: i32) -> (CertificateDer<'static>, i64, i64) {
        let mut params = CertificateParams::new(vec!["localhost".into()]).unwrap();
        params.not_before = date_time_ymd(year, 1, 1);
        params.not_after = date_time_ymd(year + 1, 1, 1);
        let der = params
            .self_signed(&KeyPair::generate().unwrap())
            .unwrap()
            .der()
            .clone();
        (
            der,
            params.not_before.unix_timestamp(),
            params.not_after.unix_timestamp(),
        )
    }

    #[test]
    fn the_leaf_must_be_valid_now_and_warns_within_fourteen_days_of_its_end() {
        let path = Path::new("chain.pem");
        for year in [2030, 2060] {
            let (der, start, end) = leaf(year);
            let check = |now| check_chain(std::slice::from_ref(&der), path, now);
            let dates = format!(
                "(valid from {year}-01-01 00:00:00Z to {}-01-01 00:00:00Z)",
                year + 1
            );
            let error = check(start - 1).unwrap_err().to_string();
            assert_eq!(
                error,
                format!("leaf certificate in \"chain.pem\" is not yet valid {dates}")
            );
            let error = check(end + 1).unwrap_err().to_string();
            assert_eq!(
                error,
                format!("leaf certificate in \"chain.pem\" is expired {dates}")
            );
            // Both ends are inside the validity period.
            assert_eq!(check(start).unwrap(), None);
            assert_eq!(check(end - 14 * DAY - 1).unwrap(), None);
            for now in [end - 14 * DAY, end - DAY, end] {
                let warning = check(now).unwrap().expect("an expiry warning");
                assert_eq!(warning.unix(), end);
                assert_eq!(warning.to_string(), format!("{}-01-01 00:00:00Z", year + 1));
            }
        }
    }

    #[test]
    fn every_certificate_in_the_chain_must_have_the_certificate_shape() {
        let path = Path::new("chain.pem");
        let (der, start, _) = leaf(2030);
        // Intermediates are not date-checked: only their shape is.
        let (old, ..) = leaf(1990);
        let chain = [der.clone(), old.clone()];
        assert_eq!(check_chain(&chain, path, start).unwrap(), None);
        for (bad, position) in [(0, "1"), (1, "2")] {
            for garbage in [&b""[..], b"\x30\x00", &old[..old.len() - 1]] {
                let mut chain = [der.clone(), old.clone()];
                chain[bad] = CertificateDer::from(garbage.to_vec());
                let error = check_chain(&chain, path, start).unwrap_err().to_string();
                assert_eq!(
                    error,
                    format!("invalid certificate {position} in \"chain.pem\"")
                );
            }
        }
    }
}
