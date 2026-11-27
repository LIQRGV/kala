//! Thin CLI wrapper for the read-only Bolt server.
use crate::cli::ServeArgs;
use crate::error::CliError;
use kala_bolt::transport::ListenAddr;
use kala_bolt::{Server, ServerConfig};
use std::io::{self, Write};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::Duration;

pub fn run(args: &ServeArgs) -> Result<String, CliError> {
    let dir = args.dir.as_path();
    let addresses = args
        .listen
        .iter()
        .map(|s| s.parse::<ListenAddr>().map_err(server_error))
        .collect::<Result<Vec<_>, _>>()?;
    let defaults = ServerConfig::default();
    let config = ServerConfig {
        listen: addresses.clone(),
        unix_mode: args.unix_mode,
        tls_cert: args.tls_cert.clone(),
        tls_key: args.tls_key.clone(),
        advertised_address: args.advertised_address.clone(),
        default_db: args.default_db.clone(),
        insecure_allow_plaintext: args.insecure_allow_plaintext,
        max_connections: args.max_connections,
        idle_timeout: Duration::from_secs(args.idle_timeout),
        read_only: args.read_only,
        acknowledge_recovery: args.acknowledge_recovery,
        max_write_transactions: args
            .max_write_transactions
            .unwrap_or(defaults.max_write_transactions),
        tx_timeout: args.tx_timeout.unwrap_or(defaults.tx_timeout),
        tx_idle_timeout: args.tx_idle_timeout.unwrap_or(defaults.tx_idle_timeout),
        busy_timeout: args.busy_timeout.unwrap_or(defaults.busy_timeout),
        ..defaults
    };
    // Never create the directory, in either auth mode.
    super::user::require_directory(dir)?;
    let stop = Arc::new(AtomicBool::new(false));
    // Block before binding so every thread inherits the mask.
    #[cfg(unix)]
    let signals = serve_signals()?;
    let server = if args.auth == "accounts" {
        Server::bind_accounts(dir, config)
    } else {
        // `--auth none` never touches accounts: no system.kala is created,
        // opened or read, and nothing is written to the directory.
        if addresses.iter().any(|addr| !addr.is_loopback()) {
            return Err(CliError::Usage(
                "no-auth server requires a loopback listen address".into(),
            ));
        }
        Server::bind(dir, config)
    }
    .map_err(server_error)?;
    #[cfg(unix)]
    handle_signals(signals, &stop, server.tls_reloader())?;
    for addr in &addresses {
        if !addr.is_loopback() && args.advertised_address.is_none() {
            eprintln!(
                "kala serve: WARNING: no --advertised-address on a non-loopback listener; routing drivers and SHOW DATABASES get each connection's local address, which may be unreachable from outside"
            );
        }
    }
    let signal_stop = Arc::clone(&stop);
    ctrlc::set_handler(move || signal_stop.store(true, Ordering::Relaxed))
        .map_err(|e| CliError::Store(format!("installing serve signal handler: {e}")))?;
    let addresses = server.local_addrs().map_err(server_error)?;
    let mut stdout = io::stdout().lock();
    for addr in addresses {
        writeln!(stdout, "kala serve: listening on {addr}").map_err(server_error)?;
    }
    stdout.flush().map_err(server_error)?;
    drop(stdout);
    server.run(stop).map_err(server_error)?;
    Ok(String::new())
}
/// Block in the main thread and use sigwait, keeping filesystem operations
/// outside signal handlers. SIGINT stays with ctrlc.
#[cfg(unix)]
fn serve_signals() -> Result<nix::sys::signal::SigSet, CliError> {
    use nix::sys::signal::{SigSet, Signal};
    let mut signals = SigSet::empty();
    signals.add(Signal::SIGTERM);
    signals.add(Signal::SIGHUP);
    signals
        .thread_block()
        .map_err(|e| CliError::Store(format!("blocking serve signals: {e}")))?;
    Ok(signals)
}

#[cfg(unix)]
fn handle_signals(
    signals: nix::sys::signal::SigSet,
    stop: &Arc<AtomicBool>,
    tls: Option<kala_bolt::transport::TlsReloader>,
) -> Result<(), CliError> {
    use nix::sys::signal::Signal;
    let stop = Arc::clone(stop);
    std::thread::Builder::new()
        .name("serve-signals".into())
        .spawn(move || {
            loop {
                match signals.wait() {
                    Ok(Signal::SIGHUP) => {
                        if let Some(tls) = &tls {
                            match tls.reload() {
                                Ok(()) => eprintln!("event=bolt_tls_reloaded"),
                                Err(e) => eprintln!("event=bolt_tls_reload_failed error={e}"),
                            }
                        }
                    }
                    Ok(_) => stop.store(true, Ordering::Relaxed),
                    Err(nix::errno::Errno::EINTR) => {}
                    Err(_) => return,
                }
            }
        })
        .map_err(|e| CliError::Store(format!("installing serve termination handling: {e}")))?;
    Ok(())
}
/// Parse only octal permission bits, never special file-mode flags.
pub(crate) fn unix_mode(value: &str) -> Result<u32, String> {
    if value.is_empty() || value.len() > 4 || !value.bytes().all(|c| (b'0'..=b'7').contains(&c)) {
        return Err("Unix mode must be octal permissions 0000 through 0777".into());
    }
    u32::from_str_radix(value, 8)
        .ok()
        .filter(|mode| *mode <= 0o777)
        .ok_or_else(|| "Unix mode must be octal permissions 0000 through 0777".into())
}
fn server_error(e: io::Error) -> CliError {
    if e.kind() == io::ErrorKind::InvalidInput {
        CliError::Usage(e.to_string())
    } else {
        CliError::io(e.kind(), e.to_string())
    }
}
