//! The actual CLI binds loopback, announces its selected port and shuts down
//! cleanly, without exposing auth tokens in either output stream.
#[path = "common/bolt.rs"]
mod bolt;
#[path = "common/serving.rs"]
mod serving;

use kala_bolt::packstream::{Value, decode, encode};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

struct Process {
    child: Child,
    output: Option<JoinHandle<String>>,
    dir: tempfile::TempDir,
    addr: SocketAddr,
}
impl Process {
    fn start(listen: &str) -> Self {
        Self::with_options(listen, &[], true)
    }
    fn with_options(listen: &str, options: &[&str], test_user: bool) -> Self {
        let dir = tempfile::tempdir().unwrap();
        drop(
            kala_fixture::load_fixture_to(
                include_str!("../../kala-core/tests/fixtures/varrow.yaml"),
                &dir.path().join("varrow.kala"),
            )
            .unwrap(),
        );
        if test_user {
            kala_system::system::add(dir.path(), "test-user@localhost", None).unwrap();
        }
        let stderr = std::fs::File::create(dir.path().join("stderr.txt")).unwrap();
        let mut child = Command::new(env!("CARGO_BIN_EXE_kala"))
            .arg("serve")
            .arg(dir.path())
            .args(["--listen", listen])
            .args(options)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(stderr)
            .spawn()
            .unwrap();
        let stdout = child.stdout.take().unwrap();
        let (tx, rx) = mpsc::channel();
        let output = thread::spawn(move || {
            let mut reader = BufReader::new(stdout);
            let mut output = String::new();
            reader.read_line(&mut output).unwrap();
            let _ = tx.send(output.clone());
            reader.read_to_string(&mut output).unwrap();
            output
        });
        let line = match rx.recv_timeout(Duration::from_secs(10)) {
            Ok(line) => line,
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                panic!("no listen line: {error}");
            }
        };
        let addr = match line
            .strip_prefix("kala serve: listening on ")
            .and_then(|s| s.trim().parse().ok())
        {
            Some(addr) => addr,
            None => {
                let _ = child.kill();
                let _ = child.wait();
                panic!("invalid startup line: {line:?}");
            }
        };
        Self {
            child,
            output: Some(output),
            dir,
            addr,
        }
    }
    #[cfg(unix)]
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
                assert_eq!(status.code(), Some(0), "SIGINT should be a clean exit");
                break;
            }
            assert!(
                Instant::now() < deadline,
                "server did not stop after SIGINT"
            );
            thread::yield_now();
        }
        let stdout = self.output.take().unwrap().join().unwrap();
        let stderr = std::fs::read_to_string(self.dir.path().join("stderr.txt")).unwrap();
        (stdout, stderr)
    }
}
impl Drop for Process {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn exchange(stream: &mut TcpStream, tag: u8, fields: Vec<Value>) -> Value {
    let bytes = encode(&Value::Structure(tag, fields)).unwrap();
    stream
        .write_all(&(bytes.len() as u16).to_be_bytes())
        .unwrap();
    stream.write_all(&bytes).unwrap();
    stream.write_all(&[0, 0]).unwrap();
    let mut response = vec![];
    loop {
        let mut header = [0; 2];
        stream.read_exact(&mut header).unwrap();
        let n = u16::from_be_bytes(header) as usize;
        if n == 0 {
            break;
        }
        let start = response.len();
        response.resize(start + n, 0);
        stream.read_exact(&mut response[start..]).unwrap();
    }
    let response = decode(&response).unwrap();
    assert!(
        matches!(response, Value::Structure(0x70, _)),
        "{response:?}"
    );
    response
}

#[test]
fn accounts_mode_logs_each_skipped_filename_once_across_startup_and_discovery() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("Before.kala"), []).unwrap();
    let server = serving::Serving::start(dir.path(), &["127.0.0.1:0"], &["--auth", "accounts"]);
    std::fs::write(dir.path().join("After.kala"), []).unwrap();
    let mut stream = TcpStream::connect(&server.addresses[0]).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    bolt::log_on(&mut stream);
    exchange(
        &mut stream,
        0x10,
        vec![
            Value::String("SHOW DATABASES".into()),
            Value::Map(Default::default()),
            Value::Map([("db".into(), Value::String("system".into()))].into()),
        ],
    );

    let log = server.stderr();
    for file in ["Before.kala", "After.kala"] {
        let event = format!("event=bolt_database_skipped name={file:?}");
        assert_eq!(log.lines().filter(|l| *l == event).count(), 1, "{event}");
    }
}

#[test]
fn non_loopback_no_auth_is_rejected_before_binding() {
    let dir = tempfile::tempdir().unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_kala"))
        .arg("serve")
        .arg(dir.path())
        .args(["--listen", "0.0.0.0:0"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8_lossy(&output.stderr).contains("loopback"));
}

#[test]
#[cfg(unix)]
fn prints_actual_listen_port_and_sigint_exits_zero() {
    let server = Process::start("127.0.0.1:0");
    assert_eq!(server.addr.ip().to_string(), "127.0.0.1");
    assert_ne!(server.addr.port(), 0);
    let (stdout, _) = server.stop();
    assert_eq!(stdout.lines().count(), 1);
}

#[test]
#[cfg(unix)]
fn localhost_maps_to_ipv4_and_basic_credentials_never_reach_output() {
    let server = Process::start("localhost:0");
    assert_eq!(server.addr.ip().to_string(), "127.0.0.1");
    let mut stream = TcpStream::connect(server.addr).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    stream
        .write_all(&[
            0x60, 0x60, 0xb0, 0x17, 0, 0, 4, 5, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        ])
        .unwrap();
    let mut version = [0; 4];
    stream.read_exact(&mut version).unwrap();
    assert_eq!(version, [0, 0, 4, 5]);
    exchange(
        &mut stream,
        0x01,
        vec![Value::Map(
            [("user_agent".into(), Value::String("test".into()))].into(),
        )],
    );
    // This is a deliberately fake regression-test password.
    let password = "s3cret-XYZ";
    exchange(
        &mut stream,
        0x6a,
        vec![Value::Map(
            [
                ("scheme".into(), Value::String("basic".into())),
                ("principal".into(), Value::String("test-user".into())),
                ("credentials".into(), Value::String(password.into())),
            ]
            .into(),
        )],
    );
    drop(stream);
    let (stdout, stderr) = server.stop();
    assert!(!stdout.contains(password));
    assert!(!stderr.contains(password));
}

#[test]
#[cfg(unix)]
fn default_accounts_bootstraps_system_and_warns_for_root_trust() {
    let server = Process::with_options("127.0.0.1:0", &[], false);
    assert!(server.dir.path().join("system.kala").is_file());
    let (_, stderr) = server.stop();
    assert!(stderr.contains("root@localhost uses trust (no password)"));
}

#[test]
#[cfg(unix)]
fn non_loopback_accounts_require_plaintext_opt_in_and_warn() {
    let server = Process::with_options(
        "0.0.0.0:0",
        &["--auth", "accounts", "--insecure-allow-plaintext"],
        false,
    );
    let (_, stderr) = server.stop();
    assert!(stderr.contains("unencrypted"), "{stderr}");
}

#[test]
fn no_auth_rejects_non_loopback_even_with_plaintext_opt_in() {
    let dir = tempfile::tempdir().unwrap();
    for options in [
        vec!["--auth", "none"],
        vec!["--auth", "none", "--insecure-allow-plaintext"],
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_kala"))
            .arg("serve")
            .arg(dir.path())
            .args(["--listen", "0.0.0.0:0"])
            .args(options)
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(2));
        assert!(String::from_utf8_lossy(&output.stderr).contains("loopback"));
    }
}

#[test]
#[cfg(unix)]
fn two_listeners_print_two_lines() {
    let server = Process::with_options(
        "127.0.0.1:0",
        &["--listen", "bolt://127.0.0.2:0", "--auth", "none"],
        false,
    );
    let (stdout, _) = server.stop();
    let addresses: Vec<SocketAddr> = stdout
        .lines()
        .map(|line| {
            line.strip_prefix("kala serve: listening on ")
                .unwrap()
                .parse()
                .unwrap()
        })
        .collect();
    assert_eq!(addresses.len(), 2, "{stdout}");
    assert_eq!(addresses[0].ip().to_string(), "127.0.0.1");
    assert_eq!(addresses[1].ip().to_string(), "127.0.0.2");
    assert!(addresses.iter().all(|address| address.port() != 0));
}

#[test]
fn serve_help_documents_write_limits_and_read_only_mode() {
    let output = Command::new(env!("CARGO_BIN_EXE_kala"))
        .args(["serve", "--help"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let help = String::from_utf8(output.stdout).unwrap();
    for flag in [
        "--read-only",
        "--max-write-transactions",
        "--tx-timeout",
        "--tx-idle-timeout",
        "--busy-timeout",
    ] {
        assert!(
            help.contains(flag),
            "serve help does not document {flag}: {help}"
        );
    }
}

#[test]
#[cfg(unix)]
fn serve_accepts_transaction_limit_flags_and_durations() {
    let server = Process::with_options(
        "127.0.0.1:0",
        &[
            "--read-only",
            "--max-write-transactions",
            "2",
            "--tx-timeout",
            "3s",
            "--tx-idle-timeout",
            "1s",
            "--busy-timeout",
            "50ms",
        ],
        false,
    );
    let (stdout, _) = server.stop();
    assert!(stdout.starts_with("kala serve: listening on "));
}
