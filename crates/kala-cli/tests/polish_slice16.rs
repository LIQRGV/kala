//! PR #17 (slice 16) review polish, through the real `kala serve` binary:
//! SIGTERM runs the clean shutdown, and the auth log names the transport.
#![cfg(unix)]

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

struct Serving {
    child: Child,
    stderr: PathBuf,
    addresses: Vec<String>,
}

impl Serving {
    fn start(dir: &Path, listeners: &[&str]) -> Self {
        let stderr = dir.join("stderr.txt");
        let mut command = Command::new(env!("CARGO_BIN_EXE_kala"));
        command.arg("serve").arg(dir).args(["--auth", "none"]);
        for listen in listeners {
            command.args(["--listen", listen]);
        }
        let mut child = command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(std::fs::File::create(&stderr).unwrap())
            .spawn()
            .unwrap();
        let stdout = child.stdout.take().unwrap();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                let _ = tx.send(line);
            }
        });
        let mut serving = Self {
            child,
            stderr,
            addresses: Vec::new(),
        };
        for _ in listeners {
            let line = rx
                .recv_timeout(Duration::from_secs(10))
                .unwrap_or_else(|e| panic!("missing startup line: {e}; {}", serving.stderr()));
            serving.addresses.push(
                line.strip_prefix("kala serve: listening on ")
                    .unwrap_or_else(|| panic!("invalid startup line {line:?}"))
                    .to_owned(),
            );
        }
        serving
    }

    fn stderr(&self) -> String {
        std::fs::read_to_string(&self.stderr).unwrap()
    }

    /// Sends `signal` and waits for the process to end.
    fn signal(&mut self, signal: &str) -> ExitStatus {
        assert!(
            Command::new("kill")
                .args([signal, &self.child.id().to_string()])
                .status()
                .unwrap()
                .success()
        );
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                return status;
            }
            assert!(
                Instant::now() < deadline,
                "{signal} did not stop the server"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}

impl Drop for Serving {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn short_dir() -> tempfile::TempDir {
    // Keep Unix sun_path use independent of a long TMPDIR.
    tempfile::Builder::new()
        .prefix("kp-")
        .tempdir_in("/tmp")
        .unwrap()
}

/// One chunked Bolt message.
fn send(stream: &mut impl Write, message: &[u8]) {
    stream
        .write_all(&(message.len() as u16).to_be_bytes())
        .unwrap();
    stream.write_all(message).unwrap();
    stream.write_all(&[0, 0]).unwrap();
}

/// Reads one chunked message and requires it to be SUCCESS.
fn success(stream: &mut impl Read) {
    let mut message = Vec::new();
    loop {
        let mut header = [0; 2];
        stream.read_exact(&mut header).unwrap();
        let len = usize::from(u16::from_be_bytes(header));
        if len == 0 {
            break;
        }
        let start = message.len();
        message.resize(start + len, 0);
        stream.read_exact(&mut message[start..]).unwrap();
    }
    assert_eq!(message[..2], [0xb1, 0x70], "expected SUCCESS: {message:?}");
}

/// Handshake (Bolt 5.4), HELLO and LOGON with scheme "none".
fn log_on(stream: &mut (impl Read + Write)) {
    stream
        .write_all(&[
            0x60, 0x60, 0xb0, 0x17, 0, 0, 4, 5, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        ])
        .unwrap();
    let mut version = [0; 4];
    stream.read_exact(&mut version).unwrap();
    assert_eq!(version, [0, 0, 4, 5]);
    // HELLO {user_agent: "test"}
    send(
        stream,
        &[
            0xb1, 0x01, 0xa1, 0x8a, b'u', b's', b'e', b'r', b'_', b'a', b'g', b'e', b'n', b't',
            0x84, b't', b'e', b's', b't',
        ],
    );
    success(stream);
    // LOGON {scheme: "none"}
    send(
        stream,
        &[
            0xb1, 0x6a, 0xa1, 0x86, b's', b'c', b'h', b'e', b'm', b'e', 0x84, b'n', b'o', b'n',
            b'e',
        ],
    );
    success(stream);
}

// `systemctl stop` and `docker stop` send SIGTERM, not SIGINT, and a closed
// terminal sends SIGHUP: both must run the same shutdown as Ctrl-C (drain,
// close, remove the socket file) and exit 0.
#[test]
fn sigterm_and_sighup_shut_down_cleanly_and_remove_the_socket() {
    for signal in ["-TERM", "-HUP"] {
        let dir = short_dir();
        let path = dir.path().join("bolt.sock");
        let unix = format!("unix://{}", path.display());
        let mut serving = Serving::start(dir.path(), &[&unix]);
        assert!(path.exists());
        let mut client = UnixStream::connect(&path).unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        log_on(&mut client);
        let status = serving.signal(signal);
        assert!(
            status.success(),
            "{signal} must exit 0 after a clean shutdown, got {status:?}; {}",
            serving.stderr()
        );
        assert!(!path.exists(), "socket file survived {signal}");
        // The open client was closed by the shutdown, not left to a killed process.
        let mut byte = [0];
        assert_eq!(client.read(&mut byte).unwrap(), 0);
        assert!(
            serving.stderr().contains("event=bolt_disconnected"),
            "{}",
            serving.stderr()
        );
    }
}

// A Unix peer has no IP: the auth log says so instead of `localhost`, and
// names the transport for both kinds of peer.
#[test]
fn the_auth_log_names_the_transport_and_gives_unix_peers_no_ip() {
    let dir = short_dir();
    let path = dir.path().join("bolt.sock");
    let unix = format!("unix://{}", path.display());
    let mut serving = Serving::start(dir.path(), &[&unix, "127.0.0.1:0"]);
    let mut local = UnixStream::connect(&path).unwrap();
    local
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    log_on(&mut local);
    let mut tcp = TcpStream::connect(&serving.addresses[1]).unwrap();
    tcp.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    log_on(&mut tcp);
    assert!(serving.signal("-INT").success());
    let stderr = serving.stderr();
    assert!(
        stderr.contains("event=bolt_authenticated connection_id=1 user=\"\" client_ip=- peer=unix"),
        "{stderr}"
    );
    assert!(
        stderr.contains(
            "event=bolt_authenticated connection_id=2 user=\"\" client_ip=127.0.0.1 peer=tcp"
        ),
        "{stderr}"
    );
    assert!(!stderr.contains("client_ip=localhost"), "{stderr}");
}
