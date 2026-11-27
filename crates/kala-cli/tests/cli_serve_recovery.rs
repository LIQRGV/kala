//! `kala serve --acknowledge-recovery`: a damaged last transaction is cut by
//! the server only when the operator opted in, never for `system`, and
//! never by a read-only server.
#![cfg(unix)]
use kala_bolt::packstream::{Value, decode, encode};
use kala_core::World;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

const VARROW: &str = include_str!("../../kala-core/tests/fixtures/varrow.yaml");

struct Serving {
    child: Child,
    dir: tempfile::TempDir,
    addr: SocketAddr,
}

impl Serving {
    fn start(options: &[&str]) -> Self {
        let dir = tempfile::tempdir().unwrap();
        drop(kala_fixture::load_fixture_to(VARROW, &dir.path().join("varrow.kala")).unwrap());
        let stderr = std::fs::File::create(dir.path().join("stderr.txt")).unwrap();
        let mut child = Command::new(env!("CARGO_BIN_EXE_kala"))
            .arg("serve")
            .arg(dir.path())
            .args(["--listen", "127.0.0.1:0", "--default-db", "varrow"])
            .args(options)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(stderr)
            .spawn()
            .unwrap();
        let mut line = String::new();
        BufReader::new(child.stdout.take().unwrap())
            .read_line(&mut line)
            .unwrap();
        let Some(addr) = line
            .strip_prefix("kala serve: listening on ")
            .and_then(|s| s.trim().parse().ok())
        else {
            let _ = child.kill();
            let _ = child.wait();
            panic!("invalid startup line: {line:?}");
        };
        Self { child, dir, addr }
    }

    fn world(&self) -> PathBuf {
        self.dir.path().join("varrow.kala")
    }

    fn stderr(&self) -> String {
        std::fs::read_to_string(self.dir.path().join("stderr.txt")).unwrap()
    }

    /// Lines of the server's log that report a cut it acknowledged.
    fn acknowledged(&self) -> Vec<String> {
        self.stderr()
            .lines()
            .filter(|line| line.starts_with("event=recovery_acknowledged "))
            .map(str::to_owned)
            .collect()
    }

    /// The quarantine files next to `world`, by name.
    fn quarantined(&self, world: &str) -> Vec<String> {
        let mut names: Vec<_> = std::fs::read_dir(self.dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|name| name.starts_with(&format!("{world}.torn-")))
            .collect();
        names.sort();
        names
    }

    fn connect(&self) -> Bolt {
        let mut stream = TcpStream::connect(self.addr).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        let mut handshake = [0_u8; 20];
        handshake[..4].copy_from_slice(&[0x60, 0x60, 0xb0, 0x17]);
        handshake[6] = 4;
        handshake[7] = 5;
        stream.write_all(&handshake).unwrap();
        let mut version = [0; 4];
        stream.read_exact(&mut version).unwrap();
        assert_eq!(version, [0, 0, 4, 5]);
        let mut bolt = Bolt { stream };
        let agent = Value::Map([("user_agent".to_owned(), Value::String("test".into()))].into());
        assert!(bolt.request(0x01, vec![agent]).0);
        let auth = Value::Map([("scheme".to_owned(), Value::String("none".into()))].into());
        assert!(bolt.request(0x6a, vec![auth]).0);
        bolt
    }
}

impl Drop for Serving {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

struct Bolt {
    stream: TcpStream,
}

impl Bolt {
    fn send(&mut self, tag: u8, fields: Vec<Value>) {
        let bytes = encode(&Value::Structure(tag, fields)).unwrap();
        self.stream
            .write_all(&(bytes.len() as u16).to_be_bytes())
            .unwrap();
        self.stream.write_all(&bytes).unwrap();
        self.stream.write_all(&[0, 0]).unwrap();
    }

    fn receive(&mut self) -> Value {
        let mut message = vec![];
        loop {
            let mut header = [0; 2];
            self.stream.read_exact(&mut header).unwrap();
            let n = u16::from_be_bytes(header) as usize;
            if n == 0 {
                break;
            }
            let start = message.len();
            message.resize(start + n, 0);
            self.stream.read_exact(&mut message[start..]).unwrap();
        }
        decode(&message).unwrap()
    }

    /// Sends a request and returns whether it succeeded, with the text of
    /// its response (records are skipped).
    fn request(&mut self, tag: u8, fields: Vec<Value>) -> (bool, String) {
        self.send(tag, fields);
        loop {
            match self.receive() {
                Value::Structure(0x71, _) => {}
                Value::Structure(tag, fields) => return (tag == 0x70, format!("{fields:?}")),
                other => panic!("unexpected response {other:?}"),
            }
        }
    }

    /// An auto-commit Datalog write: `Ok` with its summary, or the failure.
    fn write(&mut self, alias: &str) -> Result<String, String> {
        let text = format!("// kala:datalog\n[:assert :greg :alias \"{alias}\" :at 300]");
        let empty = || Value::Map(Default::default());
        let (ok, run) = self.request(0x10, vec![Value::String(text), empty(), empty()]);
        if !ok {
            assert!(self.request(0x0f, vec![]).0, "RESET");
            return Err(run);
        }
        let all = Value::Map([("n".to_owned(), Value::Integer(-1))].into());
        let (ok, summary) = self.request(0x3f, vec![all]);
        assert!(ok, "{summary}");
        Ok(summary)
    }
}

/// Appends to `path` a last transaction that is all there but fails its
/// checksum, as a crash on a file system that records the new length before
/// the data can leave it. Returns the damaged bytes. `write` makes one
/// commit on a scratch copy of the file.
fn damage_tail(path: &Path, write: impl FnOnce(&Path)) -> Vec<u8> {
    let before = std::fs::read(path).unwrap();
    let scratch = tempfile::tempdir().unwrap();
    let copy = scratch.path().join(path.file_name().unwrap());
    std::fs::write(&copy, &before).unwrap();
    write(&copy);
    let mut damaged = std::fs::read(&copy).unwrap()[before.len()..].to_vec();
    assert!(damaged.len() > 30, "the scratch write committed nothing");
    damaged[30] ^= 1;
    let mut file = std::fs::OpenOptions::new().append(true).open(path).unwrap();
    file.write_all(&damaged).unwrap();
    file.sync_all().unwrap();
    damaged
}

fn lost_write(copy: &Path) {
    let mut world = World::open(copy).unwrap();
    let greg = world.entity_by_handle("greg").unwrap();
    world
        .assert(
            greg,
            "alias",
            kala_core::Value::Str("Lost write".into()),
            Some(kala_core::Tick(301)),
        )
        .unwrap();
}

fn aliases(path: &Path) -> Vec<kala_core::Value> {
    let world = World::open_read_only(path).unwrap();
    let greg = world.entity_by_handle("greg").unwrap();
    world.as_of(greg, "alias", kala_core::Tick(300)).unwrap()
}

fn doctor_is_clean(path: &Path) {
    let output = Command::new(env!("CARGO_BIN_EXE_kala"))
        .arg("doctor")
        .arg(path)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn without_the_flag_a_damaged_tail_refuses_writes_and_stays_in_the_file() {
    let server = Serving::start(&[]);
    let path = server.world();
    damage_tail(&path, lost_write);
    let damaged_file = std::fs::read(&path).unwrap();
    let mut bolt = server.connect();
    for _ in 0..2 {
        let failure = bolt.write("Refused").unwrap_err();
        assert!(failure.contains("Neo.DatabaseError."), "{failure}");
        assert!(failure.contains("--acknowledge-recovery"), "{failure}");
        assert_eq!(std::fs::read(&path).unwrap(), damaged_file);
        assert!(server.quarantined("varrow.kala").is_empty());
    }
    assert!(server.acknowledged().is_empty());
    assert!(!server.stderr().contains("--acknowledge-recovery is on"));
}

#[test]
fn with_the_flag_the_server_cuts_a_damaged_tail_and_the_write_commits() {
    let server = Serving::start(&["--acknowledge-recovery"]);
    let path = server.world();
    // Said loudly at startup, before anything happens.
    let startup = server.stderr();
    assert!(
        startup
            .lines()
            .any(|line| line.contains("WARNING") && line.contains("--acknowledge-recovery")),
        "{startup}"
    );
    // The damaged tail is met at the writable handle's first use.
    let clean = std::fs::read(&path).unwrap();
    let damaged = damage_tail(&path, lost_write);
    let mut bolt = server.connect();
    bolt.write("After the first cut").unwrap();
    let quarantined = server.quarantined("varrow.kala");
    assert_eq!(quarantined.len(), 1, "{quarantined:?}");
    assert_eq!(
        std::fs::read(server.dir.path().join(&quarantined[0])).unwrap(),
        damaged,
        "the quarantine file holds exactly the damaged bytes"
    );
    let lines = server.acknowledged();
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert_eq!(
        lines[0],
        format!(
            "event=recovery_acknowledged db=varrow bytes={} quarantine={}",
            damaged.len(),
            quarantined[0]
        )
    );
    assert!(std::fs::read(&path).unwrap().starts_with(&clean));
    let held = aliases(&path);
    assert!(held.contains(&kala_core::Value::Str("After the first cut".into())));
    doctor_is_clean(&path);
    // More writes: nothing more is cut or logged.
    bolt.write("Another").unwrap();
    assert_eq!(server.acknowledged().len(), 1);
    // A damaged tail that appears later, under the cached handle, is cut
    // too, and that write commits as well.
    let damaged = damage_tail(&path, lost_write);
    bolt.write("After the second cut").unwrap();
    let quarantined = server.quarantined("varrow.kala");
    assert_eq!(quarantined.len(), 2, "{quarantined:?}");
    assert!(
        quarantined
            .iter()
            .any(|name| std::fs::read(server.dir.path().join(name)).unwrap() == damaged)
    );
    assert_eq!(server.acknowledged().len(), 2);
    // An explicit transaction's private handle meets one at its first write.
    damage_tail(&path, lost_write);
    assert!(bolt.request(0x11, vec![Value::Map(Default::default())]).0);
    bolt.write("In a transaction").unwrap();
    let (ok, summary) = bolt.request(0x12, vec![]);
    assert!(ok, "{summary}");
    assert_eq!(server.acknowledged().len(), 3);
    let held = aliases(&path);
    for alias in ["After the second cut", "In a transaction"] {
        assert!(
            held.contains(&kala_core::Value::Str(alias.into())),
            "{alias}"
        );
    }
    assert!(!held.contains(&kala_core::Value::Str("Lost write".into())));
    doctor_is_clean(&path);
}

#[test]
fn the_flag_never_cuts_the_system_database() {
    let server = Serving::start(&["--acknowledge-recovery"]);
    let system = server.dir.path().join("system.kala");
    damage_tail(&system, |copy| {
        kala_system::system::add(copy.parent().unwrap(), "lost@localhost", None).unwrap();
    });
    let damaged_file = std::fs::read(&system).unwrap();
    // Logging in reads `system`; writing a user world goes through the
    // writable handles the flag covers.
    let mut bolt = server.connect();
    bolt.write("System untouched").unwrap();
    let text = "// kala:datalog\n[:create :ACCOUNT :backdoor]".to_owned();
    let empty = || Value::Map(Default::default());
    let system_db = Value::Map([("db".to_owned(), Value::String("system".into()))].into());
    let (ok, refused) = bolt.request(0x10, vec![Value::String(text), empty(), system_db]);
    assert!(!ok && refused.contains("Forbidden"), "{refused}");
    assert_eq!(std::fs::read(&system).unwrap(), damaged_file);
    assert!(server.quarantined("system.kala").is_empty());
    assert!(server.acknowledged().is_empty());
}

#[test]
fn a_read_only_server_ignores_the_flag_and_says_so() {
    let server = Serving::start(&["--acknowledge-recovery", "--read-only"]);
    let path = server.world();
    let startup = server.stderr();
    assert!(
        startup
            .lines()
            .any(|line| line.contains("--acknowledge-recovery") && line.contains("ignored")),
        "{startup}"
    );
    damage_tail(&path, lost_write);
    let damaged_file = std::fs::read(&path).unwrap();
    let mut bolt = server.connect();
    let failure = bolt.write("Refused").unwrap_err();
    assert!(failure.contains("read-only"), "{failure}");
    assert_eq!(std::fs::read(&path).unwrap(), damaged_file);
    assert!(server.quarantined("varrow.kala").is_empty());
    assert!(server.acknowledged().is_empty());
}

#[test]
fn serve_help_documents_the_flag() {
    let output = Command::new(env!("CARGO_BIN_EXE_kala"))
        .args(["serve", "--help"])
        .output()
        .unwrap();
    let help = String::from_utf8(output.stdout).unwrap();
    assert!(help.contains("--acknowledge-recovery"), "{help}");
}
