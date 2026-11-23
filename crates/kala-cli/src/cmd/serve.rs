//! Thin CLI wrapper for the read-only Bolt server.
use crate::error::CliError;
use kala_bolt::{Server, ServerConfig};
use std::io::{self, Write};
use std::net::SocketAddr;
use std::path::Path;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::Duration;

pub fn run(
    dir: &Path,
    listen: &str,
    auth: &str,
    insecure_allow_plaintext: bool,
    default_db: Option<String>,
    max_connections: usize,
    idle_timeout: u64,
) -> Result<String, CliError> {
    let addr = if let Some(port) = listen.strip_prefix("localhost:") {
        SocketAddr::from((
            [127, 0, 0, 1],
            port.parse::<u16>()
                .map_err(|_| CliError::Usage("invalid listen port".into()))?,
        ))
    } else {
        listen.parse::<SocketAddr>().map_err(|_| {
            CliError::Usage("listen address must be IP:PORT, [IPv6]:PORT or localhost:PORT".into())
        })?
    };
    let config = ServerConfig {
        listen: addr,
        default_db,
        insecure_allow_plaintext,
        max_connections,
        idle_timeout: Duration::from_secs(idle_timeout),
        ..ServerConfig::default()
    };
    // Never create the directory, in either auth mode.
    super::user::require_directory(dir)?;
    let server = if auth == "accounts" {
        Server::bind_accounts(dir, config)
    } else {
        // `--auth none` never touches accounts: no system.kala is created,
        // opened or read, and nothing is written to the directory.
        if !addr.ip().is_loopback() {
            return Err(CliError::Usage(
                "no-auth server requires a loopback listen address".into(),
            ));
        }
        Server::bind(dir, config)
    }
    .map_err(server_error)?;
    let stop = Arc::new(AtomicBool::new(false));
    let signal_stop = Arc::clone(&stop);
    ctrlc::set_handler(move || signal_stop.store(true, Ordering::Relaxed))
        .map_err(|e| CliError::Store(format!("installing serve signal handler: {e}")))?;
    let addr = server.local_addr().map_err(server_error)?;
    let mut stdout = io::stdout().lock();
    writeln!(stdout, "kala serve: listening on {addr}")
        .and_then(|_| stdout.flush())
        .map_err(server_error)?;
    drop(stdout);
    server.run(stop).map_err(server_error)?;
    Ok(String::new())
}
fn server_error(e: io::Error) -> CliError {
    if e.kind() == io::ErrorKind::InvalidInput {
        CliError::Usage(e.to_string())
    } else {
        CliError::io(e.kind(), e.to_string())
    }
}
