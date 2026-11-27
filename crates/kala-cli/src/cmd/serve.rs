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
            log(format_args!(
                "kala serve: WARNING: no --advertised-address on a non-loopback listener; routing drivers and SHOW DATABASES get each connection's local address, which may be unreachable from outside"
            ));
        }
    }
    let signal_stop = Arc::clone(&stop);
    ctrlc::set_handler(move || signal_stop.store(true, Ordering::Relaxed))
        .map_err(|e| CliError::Store(format!("installing serve signal handler: {e}")))?;
    let addresses = server.local_addrs().map_err(server_error)?;
    // The listeners are bound: from here on a failed write to stdout or
    // stderr (a closed terminal or pipe) is ignored, never an exit or a panic.
    let mut lines = String::new();
    for addr in addresses {
        lines.push_str(&format!("kala serve: listening on {addr}\n"));
    }
    let mut stdout = io::stdout().lock();
    let _ = stdout.write_all(lines.as_bytes());
    let _ = stdout.flush();
    drop(stdout);
    server.run(stop).map_err(server_error)?;
    Ok(String::new())
}

/// One line on stderr. `eprintln!` panics when stderr is gone; on the signal
/// thread that would leave SIGTERM blocked in every thread with nobody waiting.
fn log(line: std::fmt::Arguments<'_>) {
    let mut text = line.to_string();
    text.push('\n');
    let _ = io::stderr().lock().write_all(text.as_bytes());
}

/// Block in the main thread and use sigwait, keeping filesystem operations
/// outside signal handlers. SIGINT stays with ctrlc.
#[cfg(unix)]
fn serve_signals() -> Result<nix::sys::signal::SigSet, CliError> {
    use nix::sys::signal::{SigSet, Signal};
    let mut signals = SigSet::empty();
    signals.add(Signal::SIGTERM);
    signals.add(Signal::SIGHUP);
    signals.add(Signal::SIGUSR1);
    signals
        .thread_block()
        .map_err(|e| CliError::Store(format!("blocking serve signals: {e}")))?;
    Ok(signals)
}

/// Sets the stop flag when the signal thread ends, however it ends: the
/// signals stay blocked in every other thread, so a server that outlived this
/// thread could no longer be stopped with SIGTERM.
#[cfg(unix)]
struct StopOnExit(Arc<AtomicBool>);
#[cfg(unix)]
impl Drop for StopOnExit {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
    }
}

/// SIGTERM and SIGHUP (a closed terminal or ssh session) run the same clean
/// shutdown as Ctrl-C. SIGUSR1 reloads the TLS certificate and key.
#[cfg(unix)]
fn handle_signals(
    signals: nix::sys::signal::SigSet,
    stop: &Arc<AtomicBool>,
    tls: Option<kala_bolt::transport::TlsReloader>,
) -> Result<(), CliError> {
    use nix::sys::signal::Signal;
    let guard = StopOnExit(Arc::clone(stop));
    let reloads = Arc::new(Coalesced::default());
    std::thread::Builder::new()
        .name("serve-signals".into())
        .spawn(move || {
            let guard = guard;
            loop {
                match signals.wait() {
                    Ok(Signal::SIGUSR1) => match &tls {
                        Some(tls) => {
                            let tls = tls.clone();
                            reloads.request(move || match tls.reload() {
                                Ok(()) => log(format_args!("event=bolt_tls_reloaded")),
                                Err(e) => {
                                    log(format_args!("event=bolt_tls_reload_failed error={e}"));
                                }
                            });
                        }
                        None => log(format_args!(
                            "event=bolt_tls_reload_ignored reason=no_tls_listener"
                        )),
                    },
                    // Keep waiting: a second SIGTERM during the drain must
                    // not fall back to the default action.
                    Ok(_) => guard.0.store(true, Ordering::Relaxed),
                    Err(nix::errno::Errno::EINTR) => {}
                    Err(_) => return,
                }
            }
        })
        .map_err(|e| CliError::Store(format!("installing serve termination handling: {e}")))?;
    Ok(())
}

/// Runs a job on a short-lived thread, at most one at a time, so the caller
/// (the signal thread) never waits for it: a certificate file on a stalled
/// mount must not keep SIGTERM from being handled. Requests made while a run
/// is in flight coalesce into one more run after it, which sees everything
/// written before the last request.
#[cfg(unix)]
#[derive(Default)]
struct Coalesced(std::sync::atomic::AtomicU8);

#[cfg(unix)]
impl Coalesced {
    const IDLE: u8 = 0;
    const RUNNING: u8 = 1;
    const AGAIN: u8 = 2;

    fn request(self: &Arc<Self>, job: impl Fn() + Send + 'static) {
        loop {
            // IDLE -> RUNNING starts a thread; RUNNING -> AGAIN asks the
            // running one for another pass; AGAIN already did.
            let swap = |from, to| {
                self.0
                    .compare_exchange(from, to, Ordering::SeqCst, Ordering::SeqCst)
            };
            if swap(Self::IDLE, Self::RUNNING).is_ok() {
                break;
            }
            match swap(Self::RUNNING, Self::AGAIN) {
                Ok(_) | Err(Self::AGAIN) => return,
                Err(_) => {}
            }
        }
        let state = Arc::clone(self);
        let spawned = std::thread::Builder::new()
            .name("serve-reload".into())
            .spawn(move || {
                loop {
                    job();
                    if state
                        .0
                        .compare_exchange(
                            Self::RUNNING,
                            Self::IDLE,
                            Ordering::SeqCst,
                            Ordering::SeqCst,
                        )
                        .is_ok()
                    {
                        return;
                    }
                    state.0.store(Self::RUNNING, Ordering::SeqCst);
                }
            });
        if spawned.is_err() {
            self.0.store(Self::IDLE, Ordering::SeqCst);
            log(format_args!(
                "event=bolt_tls_reload_failed error=could not start the reload thread"
            ));
        }
    }
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

#[cfg(all(test, unix))]
mod tests {
    use super::Coalesced;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, mpsc};
    use std::time::{Duration, Instant};

    // PR #23 polish: SIGUSR1 during a reload neither blocks the signal thread
    // nor is lost; any number of them cost one more run.
    #[test]
    fn requests_during_a_run_coalesce_into_one_more_run() {
        let gate = Arc::new(Coalesced::default());
        let runs = Arc::new(AtomicUsize::new(0));
        let (release, wait) = mpsc::channel::<()>();
        let wait = std::sync::Mutex::new(wait);
        let (started, running) = mpsc::channel();
        let counter = Arc::clone(&runs);
        let job = Arc::new(move || {
            counter.fetch_add(1, Ordering::SeqCst);
            started.send(()).unwrap();
            wait.lock().unwrap().recv().unwrap();
        });
        let request = || {
            let job = Arc::clone(&job);
            let before = Instant::now();
            gate.request(move || job());
            assert!(before.elapsed() < Duration::from_secs(1), "request waited");
        };
        request();
        running.recv_timeout(Duration::from_secs(5)).unwrap();
        // Three more while the first run is stalled.
        for _ in 0..3 {
            request();
        }
        release.send(()).unwrap();
        running.recv_timeout(Duration::from_secs(5)).unwrap();
        release.send(()).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while gate.0.load(Ordering::SeqCst) != Coalesced::IDLE {
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(runs.load(Ordering::SeqCst), 2);
        // Idle again: the next request starts a new run.
        request();
        running.recv_timeout(Duration::from_secs(5)).unwrap();
        release.send(()).unwrap();
    }
}
