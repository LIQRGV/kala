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
    let config = ServerConfig {
        listen: addresses.clone(),
        unix_mode: args.unix_mode,
        advertised_address: args.advertised_address.clone(),
        default_db: args.default_db.clone(),
        insecure_allow_plaintext: args.insecure_allow_plaintext,
        max_connections: args.max_connections,
        idle_timeout: Duration::from_secs(args.idle_timeout),
        ..ServerConfig::default()
    };
    // Never create the directory, in either auth mode.
    super::user::require_directory(dir)?;
    let stop = Arc::new(AtomicBool::new(false));
    // Before the server starts any thread: see `stop_on_termination`.
    #[cfg(unix)]
    stop_on_termination(&stop)?;
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
/// Makes SIGTERM and SIGHUP stop the server the way Ctrl-C does: `systemctl
/// stop` and `docker stop` send SIGTERM, and a default-disposition death
/// would skip the drain and leave Unix socket files behind.
///
/// The two signals are blocked in the calling thread and a dedicated thread
/// waits for them with sigwait. Threads inherit the mask of the thread that
/// starts them, so this must run before any other thread exists: afterwards
/// only the waiting thread ever receives them. No signal handler is
/// installed, and SIGINT stays with `ctrlc`.
#[cfg(unix)]
fn stop_on_termination(stop: &Arc<AtomicBool>) -> Result<(), CliError> {
    use nix::sys::signal::{SigSet, Signal};
    let failed = |e: &dyn std::fmt::Display| {
        CliError::Store(format!("installing serve termination handling: {e}"))
    };
    let mut signals = SigSet::empty();
    signals.add(Signal::SIGTERM);
    signals.add(Signal::SIGHUP);
    signals.thread_block().map_err(|e| failed(&e))?;
    let stop = Arc::clone(stop);
    std::thread::Builder::new()
        .name("serve-signals".into())
        .spawn(move || {
            loop {
                match signals.wait() {
                    Ok(_) => stop.store(true, Ordering::Relaxed),
                    Err(nix::errno::Errno::EINTR) => {}
                    Err(_) => return,
                }
            }
        })
        .map_err(|e| failed(&e))?;
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
