//! The real CLI validates and binds the whole listener set before announcing it.
#![cfg(unix)]

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::os::unix::{fs::PermissionsExt, net::UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::mpsc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

struct Running {
    child: Child,
    output: Option<JoinHandle<String>>,
    stderr: PathBuf,
    addresses: Vec<String>,
}

impl Running {
    fn start(dir: &Path, listeners: &[&str], options: &[&str]) -> Self {
        let stderr = dir.join("stderr.txt");
        let mut command = Command::new(env!("CARGO_BIN_EXE_kala"));
        command.arg("serve").arg(dir);
        for listen in listeners {
            command.args(["--listen", listen]);
        }
        let mut child = command
            .args(options)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(std::fs::File::create(&stderr).unwrap())
            .spawn()
            .unwrap();
        let stdout = child.stdout.take().unwrap();
        let (tx, rx) = mpsc::channel();
        let output = thread::spawn(move || {
            let mut output = String::new();
            for line in BufReader::new(stdout).lines() {
                let line = line.unwrap();
                output.push_str(&line);
                output.push('\n');
                let _ = tx.send(line);
            }
            output
        });
        let mut running = Self {
            child,
            output: Some(output),
            stderr,
            addresses: Vec::new(),
        };
        for _ in listeners {
            let line = rx
                .recv_timeout(Duration::from_secs(10))
                .unwrap_or_else(|error| {
                    panic!("missing startup line: {error}; {}", running.stderr());
                });
            running.addresses.push(
                line.strip_prefix("kala serve: listening on ")
                    .unwrap_or_else(|| panic!("invalid startup line {line:?}"))
                    .to_owned(),
            );
        }
        running
    }

    fn stderr(&self) -> String {
        std::fs::read_to_string(&self.stderr).unwrap()
    }

    fn stop(mut self) -> (String, String) {
        assert!(
            Command::new("kill")
                .args(["-INT", &self.child.id().to_string()])
                .status()
                .unwrap()
                .success()
        );
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                assert!(status.success(), "{}", self.stderr());
                break;
            }
            assert!(Instant::now() < deadline, "SIGINT did not stop the server");
            thread::yield_now();
        }
        let output = self.output.take().unwrap().join().unwrap();
        (output, self.stderr())
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn failed_start(dir: &Path, listeners: &[&str], options: &[&str]) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_kala"));
    command.arg("serve").arg(dir);
    for listen in listeners {
        command.args(["--listen", listen]);
    }
    let mut child = command
        .args(options)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if child.try_wait().unwrap().is_some() {
            break;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let output = child.wait_with_output().unwrap();
            panic!(
                "invalid startup did not exit: {listeners:?}; {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        thread::yield_now();
    }
    let output = child.wait_with_output().unwrap();
    assert!(
        !output.status.success(),
        "unexpectedly started: {listeners:?}"
    );
    assert!(
        output.stdout.is_empty(),
        "startup must announce nothing on failure"
    );
    output
}

fn negotiate(stream: &mut (impl Read + Write)) {
    stream
        .write_all(&[
            0x60, 0x60, 0xb0, 0x17, 0, 0, 4, 5, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        ])
        .unwrap();
    let mut version = [0; 4];
    stream.read_exact(&mut version).unwrap();
    assert_eq!(version, [0, 0, 4, 5]);
}

fn short_dir() -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix("kt-")
        .tempdir_in("/tmp")
        .unwrap()
}

#[test]
fn each_tcp_spelling_binds_the_requested_host() {
    for (listen, host) in [
        ("127.0.0.1:0", "127.0.0.1"),
        ("localhost:0", "127.0.0.1"),
        ("[::1]:0", "::1"),
        ("bolt://127.0.0.1:0", "127.0.0.1"),
        ("bolt://localhost:0", "127.0.0.1"),
        ("bolt://[::1]:0", "::1"),
    ] {
        let dir = short_dir();
        let running = Running::start(dir.path(), &[listen], &["--auth", "none"]);
        let address: SocketAddr = running.addresses[0].parse().unwrap();
        assert_eq!(address.ip().to_string(), host);
        assert_ne!(address.port(), 0);
        let mut stream = TcpStream::connect(address).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        negotiate(&mut stream);
        let (output, _) = running.stop();
        assert_eq!(output.lines().count(), 1);
        assert!(
            TcpListener::bind(address).is_ok(),
            "listener survived SIGINT"
        );
    }
}

#[test]
fn mixed_listener_startup_preserves_order_and_sigint_removes_socket() {
    for unix_first in [false, true] {
        let dir = short_dir();
        let path = dir.path().join("bolt.sock");
        let unix = format!("unix://{}", path.display());
        let listeners = if unix_first {
            vec![unix.as_str(), "bolt://127.0.0.1:0"]
        } else {
            vec!["bolt://127.0.0.1:0", unix.as_str()]
        };
        let running = Running::start(dir.path(), &listeners, &["--auth", "none"]);
        let unix_index = usize::from(!unix_first);
        assert_eq!(running.addresses[unix_index], unix);
        let tcp: SocketAddr = running.addresses[1 - unix_index].parse().unwrap();
        let mut tcp_stream = TcpStream::connect(tcp).unwrap();
        tcp_stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut unix_stream = UnixStream::connect(&path).unwrap();
        unix_stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        negotiate(&mut tcp_stream);
        negotiate(&mut unix_stream);
        let (output, _) = running.stop();
        assert_eq!(output.lines().count(), 2);
        assert!(!path.exists(), "socket file survived clean shutdown");
        let mut byte = [0];
        assert_eq!(tcp_stream.read(&mut byte).unwrap(), 0);
        assert_eq!(unix_stream.read(&mut byte).unwrap(), 0);
    }
}

#[test]
fn unix_only_allows_both_auth_modes_and_sets_exact_permissions() {
    for (auth, mode, expected) in [
        ("none", None, 0o600),
        ("accounts", None, 0o600),
        ("none", Some("0660"), 0o660),
        ("none", Some("0666"), 0o666),
    ] {
        let dir = short_dir();
        let path = dir.path().join("bolt.sock");
        let unix = format!("unix://{}", path.display());
        let mut options = vec!["--auth", auth];
        if let Some(mode) = mode {
            options.extend(["--unix.mode", mode]);
        }
        let running = Running::start(dir.path(), &[&unix], &options);
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o7777,
            expected
        );
        assert_eq!(running.addresses, [unix]);
        assert_eq!(dir.path().join("system.kala").exists(), auth == "accounts");
        let (_, stderr) = running.stop();
        assert!(!stderr.contains("unencrypted"), "{stderr}");
        assert!(!stderr.contains("non-loopback"), "{stderr}");
        assert!(!path.exists());
    }
}

#[test]
fn malformed_specs_and_modes_fail_without_startup_output() {
    let dir = short_dir();
    for (listen, expected) in [
        ("unix://relative.sock", "absolute"),
        ("unix://", "absolute"),
        ("ws://localhost:0", "ws"),
        ("wss://localhost:0", "wss"),
        ("bolt+s://localhost:0", "bolt+s"),
        ("bolt+ssc://localhost:0", "bolt+ssc"),
        ("neo4j://localhost:0", "not a listen address"),
        ("bad-scheme://localhost:0", "bad-scheme"),
        ("example.com:0", "listen"),
    ] {
        let output = failed_start(dir.path(), &[listen], &["--auth", "none"]);
        let error = String::from_utf8_lossy(&output.stderr);
        assert!(error.contains(expected), "{listen}: {error}");
    }
    let long = format!("unix:///{}", "x".repeat(200));
    let output = failed_start(dir.path(), &[&long], &["--auth", "none"]);
    assert!(String::from_utf8_lossy(&output.stderr).contains("limit"));
    for mode in ["0680", "-1", "1000", "888", "0x600", "word"] {
        let output = failed_start(
            dir.path(),
            &["127.0.0.1:0"],
            &["--auth", "none", &format!("--unix.mode={mode}")],
        );
        let error = String::from_utf8_lossy(&output.stderr);
        assert!(
            error.contains("mode") || error.contains("octal"),
            "{mode}: {error}"
        );
    }
}

#[test]
fn duplicates_and_seventeen_listeners_are_rejected() {
    let dir = short_dir();
    for addresses in [
        vec!["127.0.0.1:0", "127.0.0.1:0"],
        vec!["localhost:0", "bolt://127.0.0.1:0"],
    ] {
        let output = failed_start(dir.path(), &addresses, &["--auth", "none"]);
        assert!(String::from_utf8_lossy(&output.stderr).contains("duplicate"));
    }
    let addresses: Vec<_> = (1..=17).map(|n| format!("127.0.0.{n}:0")).collect();
    let refs: Vec<_> = addresses.iter().map(String::as_str).collect();
    let output = failed_start(dir.path(), &refs, &["--auth", "none"]);
    assert!(String::from_utf8_lossy(&output.stderr).contains("16"));
}

#[test]
fn failed_later_bind_rolls_back_tcp_and_unix_before_announcing() {
    let dir = short_dir();
    let path = dir.path().join("bolt.sock");
    let unix = format!("unix://{}", path.display());
    let available = TcpListener::bind("127.0.0.1:0").unwrap();
    let available_address = available.local_addr().unwrap();
    drop(available);
    let occupied = TcpListener::bind("127.0.0.1:0").unwrap();
    let occupied_address = occupied.local_addr().unwrap();
    let output = failed_start(
        dir.path(),
        &[
            &available_address.to_string(),
            &unix,
            &occupied_address.to_string(),
        ],
        &["--auth", "none"],
    );
    assert!(String::from_utf8_lossy(&output.stderr).contains(&occupied_address.to_string()));
    assert!(!path.exists());
    assert!(TcpListener::bind(available_address).is_ok());
    assert!(
        std::fs::read_dir(dir.path()).unwrap().next().is_none(),
        "private bind directory leaked"
    );
}

#[test]
fn unix_listener_never_weakens_tcp_auth_or_plaintext_rules() {
    for unix_first in [false, true] {
        for options in [
            vec!["--auth", "none"],
            vec!["--auth", "none", "--insecure-allow-plaintext"],
            vec!["--auth", "accounts"],
        ] {
            let dir = short_dir();
            let path = dir.path().join("bolt.sock");
            let unix = format!("unix://{}", path.display());
            let listeners = if unix_first {
                vec![unix.as_str(), "0.0.0.0:0"]
            } else {
                vec!["0.0.0.0:0", unix.as_str()]
            };
            let output = failed_start(dir.path(), &listeners, &options);
            assert!(String::from_utf8_lossy(&output.stderr).contains("loopback"));
            assert!(!path.exists());
        }
    }
}

#[test]
fn sixteen_listeners_are_served_and_announced() {
    let dir = short_dir();
    let listeners: Vec<_> = (1..=16).map(|n| format!("127.0.0.{n}:0")).collect();
    let refs: Vec<_> = listeners.iter().map(String::as_str).collect();
    let running = Running::start(dir.path(), &refs, &["--auth", "none"]);
    for (index, address) in running.addresses.iter().enumerate() {
        let address: SocketAddr = address.parse().unwrap();
        assert_eq!(address.ip().to_string(), format!("127.0.0.{}", index + 1));
        let mut stream = TcpStream::connect(address).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        negotiate(&mut stream);
    }
    let (output, _) = running.stop();
    assert_eq!(output.lines().count(), 16);
}

#[test]
fn every_non_loopback_listener_retains_the_advertised_address_warning() {
    let dir = short_dir();
    let path = dir.path().join("bolt.sock");
    let unix = format!("unix://{}", path.display());
    let running = Running::start(
        dir.path(),
        &[&unix, "0.0.0.0:0", "[::]:0"],
        &["--insecure-allow-plaintext"],
    );
    let (_, stderr) = running.stop();
    assert_eq!(
        stderr
            .matches("no --advertised-address on a non-loopback listener")
            .count(),
        2,
        "{stderr}"
    );
}
