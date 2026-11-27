//! A bounded harness for real CLI server and Unix signal tests.
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

pub struct Serving {
    child: Child,
    stderr: PathBuf,
    pub addresses: Vec<String>,
}

impl Serving {
    pub fn start(dir: &Path, listeners: &[&str], options: &[&str]) -> Self {
        let stderr = dir.join("stderr.txt");
        let mut command = Command::new(env!("CARGO_BIN_EXE_kala"));
        command.arg("serve").arg(dir).args(options);
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

    pub fn stderr(&self) -> String {
        std::fs::read_to_string(&self.stderr).unwrap()
    }

    pub fn send_signal(&self, signal: &str) {
        assert!(
            Command::new("kill")
                .args([signal, &self.child.id().to_string()])
                .status()
                .unwrap()
                .success()
        );
    }

    /// Sends `signal` and waits for the process to end.
    pub fn signal(&mut self, signal: &str) -> ExitStatus {
        self.send_signal(signal);
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

pub fn short_dir() -> tempfile::TempDir {
    // Keep Unix sun_path use independent of a long TMPDIR.
    tempfile::Builder::new()
        .prefix("kp-")
        .tempdir_in("/tmp")
        .unwrap()
}
