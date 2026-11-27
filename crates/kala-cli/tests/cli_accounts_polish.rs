//! PR #5 polish round: regression tests for `kala user` / `kala serve`
//! directory handling, `--auth none` isolation from accounts,
//! `--acknowledge-recovery`, native write reports and clap diagnostics.
//!
//! Tests marked GUARD pass today and pin behaviour that must be kept.

mod common;
#[path = "common/damage.rs"]
mod damage;

use common::{VARROW, code, feed, kala, stderr, stdout};
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

const SECRET: &str = "SECRET-UNIQUE-123";

fn fixture() -> String {
    fs::read_to_string(VARROW).unwrap()
}

fn varrow_dir() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    drop(kala_fixture::load_fixture_to(&fixture(), &dir.path().join("varrow.kala")).unwrap());
    dir
}

/// Runs `kala args...` with stdin closed and a hard deadline (a server that
/// wrongly starts is killed rather than hanging the test). Output goes to
/// files outside the directory under test.
fn bounded(args: &[&str]) -> Output {
    let scratch = tempfile::tempdir().unwrap();
    let out = fs::File::create(scratch.path().join("out")).unwrap();
    let err = fs::File::create(scratch.path().join("err")).unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_kala"))
        .args(args)
        .stdin(Stdio::null())
        .stdout(out)
        .stderr(err)
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(8);
    let status = loop {
        if let Some(s) = child.try_wait().unwrap() {
            break s;
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            break child.wait().unwrap();
        }
        thread::yield_now();
    };
    Output {
        status,
        stdout: fs::read(scratch.path().join("out")).unwrap(),
        stderr: fs::read(scratch.path().join("err")).unwrap(),
    }
}

fn user(args: &[&str]) -> Output {
    let mut all = vec!["user"];
    all.extend_from_slice(args);
    bounded(&all)
}

fn ok(o: &Output) {
    assert_eq!(code(o), 0, "{}", stderr(o));
}

/// File names (recursive, sorted) with the bytes of every file.
fn tree(dir: &Path) -> Vec<(String, Vec<u8>)> {
    fn walk(base: &Path, dir: &Path, out: &mut Vec<(String, Vec<u8>)>) {
        for e in fs::read_dir(dir).unwrap() {
            let e = e.unwrap();
            let p = e.path();
            let rel = p.strip_prefix(base).unwrap().display().to_string();
            if p.is_dir() {
                out.push((format!("{rel}/"), vec![]));
                walk(base, &p, out);
            } else {
                out.push((rel, fs::read(&p).unwrap_or_default()));
            }
        }
    }
    let mut out = vec![];
    walk(dir, dir, &mut out);
    out.sort();
    out
}

/// A running `kala serve`, killed on drop; stderr goes to a file.
struct Serve {
    child: Child,
    _scratch: tempfile::TempDir,
    err_path: PathBuf,
    /// The first stdout line ("" if the process exited without one).
    first_line: String,
}
impl Serve {
    fn start(dir: &Path, extra: &[&str]) -> Self {
        let scratch = tempfile::tempdir().unwrap();
        let err_path = scratch.path().join("err");
        let mut child = Command::new(env!("CARGO_BIN_EXE_kala"))
            .arg("serve")
            .arg(dir)
            .args(["--listen", "127.0.0.1:0"])
            .args(extra)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(fs::File::create(&err_path).unwrap())
            .spawn()
            .unwrap();
        let stdout = child.stdout.take().unwrap();
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let mut line = String::new();
            let _ = BufReader::new(stdout).read_line(&mut line);
            let _ = tx.send(line);
        });
        let first_line = rx.recv_timeout(Duration::from_secs(10)).unwrap_or_default();
        Self {
            child,
            _scratch: scratch,
            err_path,
            first_line,
        }
    }
    fn listening(&self) -> bool {
        self.first_line.starts_with("kala serve: listening on ")
    }
    fn stderr(&self) -> String {
        fs::read_to_string(&self.err_path).unwrap_or_default()
    }
}
impl Drop for Serve {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// True when chmod does not restrict us (running as root).
#[cfg(unix)]
fn chmod_ineffective(dir: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(dir, fs::Permissions::from_mode(0o555)).unwrap();
    let probe = dir.join(".probe");
    let writable = fs::write(&probe, b"x").is_ok();
    let _ = fs::remove_file(&probe);
    fs::set_permissions(dir, fs::Permissions::from_mode(0o755)).unwrap();
    writable
}

// ---------------------------------------------------------------- Item 1

fn missing_dir_cases() -> Vec<(&'static str, Vec<&'static str>)> {
    vec![
        (
            "user add",
            vec!["user", "add", "@D", "a@localhost", "--trust"],
        ),
        (
            "user set",
            vec!["user", "set", "@D", "a@localhost", "--trust"],
        ),
        (
            "user grant",
            vec!["user", "grant", "@D", "a@localhost", "read", "x"],
        ),
        (
            "user revoke",
            vec!["user", "revoke", "@D", "a@localhost", "read", "x"],
        ),
        ("user drop", vec!["user", "drop", "@D", "a@localhost"]),
        ("user list", vec!["user", "list", "@D"]),
        (
            "serve accounts",
            vec![
                "serve",
                "@D",
                "--listen",
                "127.0.0.1:0",
                "--auth",
                "accounts",
            ],
        ),
        (
            "serve none",
            vec!["serve", "@D", "--listen", "127.0.0.1:0", "--auth", "none"],
        ),
    ]
}

#[test]
fn missing_directory_is_an_error_and_is_never_created_by_user_or_serve() {
    for (what, args) in missing_dir_cases() {
        let parent = tempfile::tempdir().unwrap();
        let missing = parent.path().join("nope");
        let missing_s = missing.to_str().unwrap();
        let args: Vec<&str> = args
            .iter()
            .map(|a| if *a == "@D" { missing_s } else { a })
            .collect();
        let o = bounded(&args);
        assert_ne!(code(&o), 0, "{what}: should fail, stdout={}", stdout(&o));
        assert!(
            stderr(&o).to_lowercase().contains("no such directory"),
            "{what}: stderr={}",
            stderr(&o)
        );
        assert!(!missing.exists(), "{what}: created the missing directory");
        assert!(
            fs::read_dir(parent.path()).unwrap().next().is_none(),
            "{what}: wrote next to the missing directory"
        );
    }
}

// GUARD: only system.kala inside an existing directory may be created.
#[test]
fn guard_user_add_on_existing_empty_directory_still_bootstraps() {
    let dir = tempfile::tempdir().unwrap();
    ok(&user(&[
        "add",
        dir.path().to_str().unwrap(),
        "a@localhost",
        "--trust",
    ]));
    assert!(dir.path().join("system.kala").is_file());
    let names: Vec<_> = fs::read_dir(dir.path())
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(names, ["system.kala"], "no staging leftovers");
    assert!(stdout(&user(&["list", dir.path().to_str().unwrap()])).contains("a@localhost"));
}

// ---------------------------------------------------------------- Item 2

#[test]
fn auth_none_serve_writes_nothing_to_the_directory() {
    let dir = varrow_dir();
    let before = tree(dir.path());
    {
        let s = Serve::start(dir.path(), &["--auth", "none"]);
        assert!(
            s.listening(),
            "line={:?} stderr={}",
            s.first_line,
            s.stderr()
        );
        assert!(
            !dir.path().join("system.kala").exists(),
            "--auth none created system.kala"
        );
    }
    assert_eq!(tree(dir.path()), before, "directory listing changed");
}

#[test]
#[cfg(unix)]
fn auth_none_serve_starts_on_a_read_only_directory() {
    use std::os::unix::fs::PermissionsExt;
    let dir = varrow_dir();
    if chmod_ineffective(dir.path()) {
        eprintln!("skipped: chmod does not restrict this user (root)");
        return;
    }
    fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o555)).unwrap();
    let s = Serve::start(dir.path(), &["--auth", "none"]);
    let listening = s.listening();
    let line = s.first_line.clone();
    let err = s.stderr();
    drop(s);
    fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o755)).unwrap();
    assert!(listening, "line={line:?} stderr={err}");
}

#[test]
fn auth_none_serve_does_not_read_an_invalid_system_kala() {
    let dir = varrow_dir();
    fs::write(dir.path().join("system.kala"), b"this is not a kala world").unwrap();
    let before = tree(dir.path());
    let s = Serve::start(dir.path(), &["--auth", "none"]);
    assert!(
        s.listening(),
        "line={:?} stderr={}",
        s.first_line,
        s.stderr()
    );
    drop(s);
    assert_eq!(tree(dir.path()), before);
}

#[test]
#[cfg(unix)]
fn auth_none_serve_does_not_read_an_unreadable_system_kala() {
    use std::os::unix::fs::PermissionsExt;
    let dir = varrow_dir();
    // A real system world, then made unreadable.
    ok(&user(&["list", dir.path().to_str().unwrap()]));
    let path = dir.path().join("system.kala");
    fs::set_permissions(&path, fs::Permissions::from_mode(0o000)).unwrap();
    if fs::read(&path).is_ok() {
        eprintln!("skipped: chmod does not restrict this user (root)");
        return;
    }
    let s = Serve::start(dir.path(), &["--auth", "none"]);
    let listening = s.listening();
    let (line, err) = (s.first_line.clone(), s.stderr());
    drop(s);
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    assert!(listening, "line={line:?} stderr={err}");
}

#[test]
fn auth_none_serve_skips_a_user_world_named_system_with_one_stderr_line() {
    let dir = tempfile::tempdir().unwrap();
    drop(kala_fixture::load_fixture_to(&fixture(), &dir.path().join("system.kala")).unwrap());
    drop(kala_fixture::load_fixture_to(&fixture(), &dir.path().join("varrow.kala")).unwrap());
    let before = tree(dir.path());
    let s = Serve::start(dir.path(), &["--auth", "none"]);
    assert!(
        s.listening(),
        "line={:?} stderr={}",
        s.first_line,
        s.stderr()
    );
    let mentions: Vec<String> = s
        .stderr()
        .lines()
        .filter(|l| l.contains("system.kala"))
        .map(str::to_string)
        .collect();
    drop(s);
    assert_eq!(
        mentions.len(),
        1,
        "exactly one stderr line must mention skipping system.kala: {mentions:?}"
    );
    assert!(
        mentions[0].to_lowercase().contains("skip"),
        "the line must say it was skipped: {mentions:?}"
    );
    assert_eq!(tree(dir.path()), before);
}

// ---------------------------------------------------------------- Item 4

const WRITE_WITH_FLAG: [(&str, &[&str]); 5] = [
    ("add", &["bob@localhost", "--trust"]),
    ("set", &["bob@localhost", "--trust"]),
    ("grant", &["bob@localhost", "read", "varrow"]),
    ("revoke", &["bob@localhost", "read", "varrow"]),
    ("drop", &["bob@localhost"]),
];

#[test]
fn every_user_write_subcommand_accepts_acknowledge_recovery() {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path().to_str().unwrap();
    ok(&user(&["list", d]));
    for (action, rest) in WRITE_WITH_FLAG {
        let mut args = vec![action, d];
        args.extend_from_slice(rest);
        args.push("--acknowledge-recovery");
        let o = user(&args);
        assert_eq!(code(&o), 0, "{action}: {}", stderr(&o));
    }
}

/// A system world whose last transaction (a grant) is fully present but
/// fails its checksum. Returns (dir, bytes before damage end, damaged bytes,
/// offset where the damaged transaction starts).
fn damaged_system() -> (tempfile::TempDir, Vec<u8>, usize) {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path().to_str().unwrap();
    ok(&user(&["add", d, "alice@localhost", "--trust"]));
    ok(&user(&["grant", d, "alice@localhost", "read", "varrow"]));
    let path = dir.path().join("system.kala");
    let mut bytes = fs::read(&path).unwrap();
    let last = damage::spans(&bytes).pop().unwrap();
    bytes[last.body_start + 6] ^= 0x01;
    fs::write(&path, &bytes).unwrap();
    (dir, bytes, last.start)
}

#[test]
fn user_write_refuses_a_damaged_tail_and_names_the_flag_without_touching_the_file() {
    let (dir, _, _) = damaged_system();
    let d = dir.path().to_str().unwrap();
    let before = tree(dir.path());
    for _ in 0..2 {
        let o = user(&["add", d, "bob@localhost", "--trust"]);
        assert_ne!(code(&o), 0, "{}", stdout(&o));
        assert!(
            stderr(&o).contains("--acknowledge-recovery"),
            "stderr={}",
            stderr(&o)
        );
        assert_eq!(
            tree(dir.path()),
            before,
            "refused write must not cut or quarantine"
        );
    }
}

#[test]
fn user_write_with_acknowledge_recovery_quarantines_the_tail_then_applies() {
    let (dir, damaged, start) = damaged_system();
    let d = dir.path().to_str().unwrap();
    let o = user(&[
        "add",
        d,
        "bob@localhost",
        "--trust",
        "--acknowledge-recovery",
    ]);
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    let listing = stdout(&user(&["list", d]));
    assert!(listing.contains("bob@localhost"), "{listing}");
    assert!(listing.contains("alice@localhost"), "{listing}");
    assert!(
        !listing.contains("read:varrow"),
        "the damaged grant must have been cut: {listing}"
    );
    // Same quarantine naming and content as the core recovery used by
    // `kala transact --acknowledge-recovery`: `<file>.torn-<valid_len>`.
    let q = dir.path().join(format!("system.kala.torn-{start}"));
    assert_eq!(
        fs::read(&q).unwrap(),
        damaged[start..],
        "quarantine content"
    );
    let extra: Vec<_> = fs::read_dir(dir.path())
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|n| n.contains(".torn-"))
        .collect();
    assert_eq!(extra.len(), 1, "{extra:?}");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(&q).unwrap().permissions().mode() & 0o777,
            0o600,
            "quarantine keeps the system world's private mode"
        );
    }
    // The log keeps the valid prefix.
    assert!(
        fs::read(dir.path().join("system.kala"))
            .unwrap()
            .starts_with(&damaged[..start])
    );
}

// ---------------------------------------------------------------- Item 6

fn next_tick(dir: &Path) -> i64 {
    kala_core::World::open_read_only(&dir.join("system.kala"))
        .unwrap()
        .latest_tick()
        .0
        + 1
}

#[test]
fn user_grant_and_revoke_report_changed_count_and_tick_and_noops_report_zero() {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path().to_str().unwrap();
    ok(&user(&["add", d, "alice@localhost", "--trust"]));

    let tick = next_tick(dir.path());
    let o = user(&["grant", d, "alice@localhost", "read", "varrow"]);
    ok(&o);
    assert!(
        stdout(&o).contains(&format!("1 changed, written at tick {tick}")),
        "stdout={}",
        stdout(&o)
    );
    assert!(!stdout(&o).contains("account updated"));

    let o = user(&["grant", d, "alice@localhost", "read", "varrow"]);
    ok(&o);
    assert!(stdout(&o).contains("0 changed"), "stdout={}", stdout(&o));
    assert!(!stdout(&o).contains("1 changed"), "stdout={}", stdout(&o));
    assert!(!stdout(&o).contains("account updated"));

    let tick = next_tick(dir.path());
    let o = user(&["revoke", d, "alice@localhost", "read", "varrow"]);
    ok(&o);
    assert!(
        stdout(&o).contains(&format!("1 changed, written at tick {tick}")),
        "stdout={}",
        stdout(&o)
    );

    let o = user(&["revoke", d, "alice@localhost", "write", "varrow"]);
    ok(&o);
    assert!(stdout(&o).contains("0 changed"), "stdout={}", stdout(&o));
    assert!(!stdout(&o).contains("account updated"));
}

#[test]
fn user_add_set_drop_report_the_native_write_result_not_account_updated() {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path().to_str().unwrap();
    ok(&user(&["list", d]));
    for args in [
        vec!["add", d, "bob@localhost", "--trust"],
        vec!["set", d, "bob@localhost", "--trust"],
        vec!["drop", d, "bob@localhost"],
    ] {
        let tick = next_tick(dir.path());
        let o = user(&args);
        ok(&o);
        let text = stdout(&o);
        assert!(!text.contains("account updated"), "{args:?}: {text}");
        assert!(
            text.contains(&format!(" changed, written at tick {tick}")),
            "{args:?}: {text}"
        );
    }
    // `set` to the same trust plugin changes nothing.
    let dir2 = tempfile::tempdir().unwrap();
    let d2 = dir2.path().to_str().unwrap();
    ok(&user(&["add", d2, "bob@localhost", "--trust"]));
    let o = user(&["set", d2, "bob@localhost", "--trust"]);
    ok(&o);
    assert!(stdout(&o).contains("0 changed"), "stdout={}", stdout(&o));
}

// ---------------------------------------------------------------- Item 7

fn assert_clap_error_without_secret(o: &Output) {
    assert_eq!(code(o), 2, "stderr={}", stderr(o));
    assert!(!stdout(o).contains(SECRET));
    assert!(!stderr(o).contains(SECRET), "secret echoed: {}", stderr(o));
}

#[test]
fn user_missing_argument_error_names_the_argument() {
    let dir = tempfile::tempdir().unwrap();
    let o = kala(["user", "add", dir.path().to_str().unwrap()]);
    assert_eq!(code(&o), 2);
    assert!(
        stderr(&o).contains("<ACCOUNT>"),
        "clap's diagnosis must survive: {}",
        stderr(&o)
    );
}

#[test]
fn user_unknown_subcommand_error_names_it() {
    let o = kala(["user", "frobnicate"]);
    assert_eq!(code(&o), 2);
    let text = stderr(&o);
    assert!(
        text.contains("unrecognized subcommand") && text.contains("<redacted>"),
        "{text}"
    );
    assert!(!text.contains("frobnicate"), "{text}");
}

#[test]
fn user_password_equals_flag_is_diagnosed_without_echoing_the_value() {
    let dir = tempfile::tempdir().unwrap();
    let o = kala([
        "user",
        "add",
        dir.path().to_str().unwrap(),
        "a@localhost",
        &format!("--password={SECRET}"),
    ]);
    assert_clap_error_without_secret(&o);
    assert!(stderr(&o).contains("unexpected argument"), "{}", stderr(&o));
}

#[test]
fn user_separate_password_flag_and_value_is_diagnosed_without_echoing_the_value() {
    let dir = tempfile::tempdir().unwrap();
    let o = kala([
        "user",
        "add",
        dir.path().to_str().unwrap(),
        "a@localhost",
        "--password",
        SECRET,
    ]);
    assert_clap_error_without_secret(&o);
    assert!(stderr(&o).contains("unexpected argument"), "{}", stderr(&o));
}

#[test]
fn user_extra_positional_is_diagnosed_without_echoing_the_value() {
    let dir = tempfile::tempdir().unwrap();
    let o = kala([
        "user",
        "add",
        dir.path().to_str().unwrap(),
        "a@localhost",
        "--trust",
        SECRET,
    ]);
    assert_clap_error_without_secret(&o);
    assert!(stderr(&o).contains("unexpected argument"), "{}", stderr(&o));
}

// ------------------------------------------------- Edge cases (confirm round)

const NO_LEAK: [&str; 3] = ["$argon2", "password_hash", "SECRET-UNIQUE"];

fn assert_no_hash(o: &Output) {
    for needle in NO_LEAK {
        assert!(
            !stdout(o).contains(needle) && !stderr(o).contains(needle),
            "`{needle}` leaked: out={} err={}",
            stdout(o),
            stderr(o)
        );
    }
}

#[test]
fn dir_that_is_a_regular_file_is_refused_and_nothing_is_created() {
    for (what, args) in missing_dir_cases() {
        let parent = tempfile::tempdir().unwrap();
        let file = parent.path().join("afile");
        fs::write(&file, b"keep me").unwrap();
        let before = tree(parent.path());
        let file_s = file.to_str().unwrap();
        let args: Vec<&str> = args
            .iter()
            .map(|a| if *a == "@D" { file_s } else { a })
            .collect();
        let o = bounded(&args);
        assert_ne!(code(&o), 0, "{what}: stdout={}", stdout(&o));
        assert_eq!(tree(parent.path()), before, "{what}: changed the tree");
        assert_eq!(fs::read(&file).unwrap(), b"keep me", "{what}");
    }
}

#[test]
fn acknowledge_recovery_on_a_clean_world_applies_without_quarantine() {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path().to_str().unwrap();
    ok(&user(&["add", d, "alice@localhost", "--trust"]));
    let o = user(&[
        "add",
        d,
        "bob@localhost",
        "--trust",
        "--acknowledge-recovery",
    ]);
    ok(&o);
    assert_no_hash(&o);
    assert!(stdout(&o).contains(" changed, written at tick "));
    assert!(stdout(&user(&["list", d])).contains("bob@localhost"));
    assert_eq!(common::dir_names(dir.path()), vec!["system.kala"]);
}

#[test]
fn acknowledge_recovery_on_a_torn_incomplete_tail_cuts_it_and_applies() {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path().to_str().unwrap();
    ok(&user(&["add", d, "alice@localhost", "--trust"]));
    ok(&user(&["grant", d, "alice@localhost", "read", "varrow"]));
    let path = dir.path().join("system.kala");
    let mut bytes = fs::read(&path).unwrap();
    let last = damage::spans(&bytes).pop().unwrap();
    bytes.truncate(last.end - 10); // incomplete, not checksum-damaged
    fs::write(&path, &bytes).unwrap();
    let o = user(&[
        "add",
        d,
        "bob@localhost",
        "--trust",
        "--acknowledge-recovery",
    ]);
    ok(&o);
    assert_no_hash(&o);
    let listing = stdout(&user(&["list", d]));
    assert!(listing.contains("bob@localhost"), "{listing}");
    assert!(
        !listing.contains("read:varrow"),
        "torn grant kept: {listing}"
    );
    let q = dir.path().join(format!("system.kala.torn-{}", last.start));
    assert_eq!(fs::read(&q).unwrap(), bytes[last.start..]);
}

#[test]
fn grant_with_invalid_account_name_on_damaged_world_cuts_nothing() {
    let (dir, _, _) = damaged_system();
    let d = dir.path().to_str().unwrap();
    let before = tree(dir.path());
    let o = user(&[
        "grant",
        d,
        "bad name",
        "read",
        "varrow",
        "--acknowledge-recovery",
    ]);
    assert_ne!(code(&o), 0, "{}", stdout(&o));
    assert_no_hash(&o);
    assert_eq!(tree(dir.path()), before, "rejected command must not cut");
}

#[test]
fn user_password_flag_before_subcommand_is_diagnosed_without_echoing_the_value() {
    let dir = tempfile::tempdir().unwrap();
    let o = kala([
        "user",
        "--password=SECRET-UNIQUE-456",
        "add",
        dir.path().to_str().unwrap(),
        "a@localhost",
    ]);
    assert_eq!(code(&o), 2, "stderr={}", stderr(&o));
    assert!(!stdout(&o).contains("SECRET-UNIQUE-456"));
    assert!(!stderr(&o).contains("SECRET-UNIQUE-456"), "{}", stderr(&o));
    assert!(common::dir_names(dir.path()).is_empty());
}

// ---------------------------------------------------------------- Item 8

/// Runs `kala repl --write world [extra]` with `input` on stdin.
fn repl_write(world: &Path, extra: &[&str], input: &str) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_kala"))
        .args(["repl", "--write"])
        .arg(world)
        .args(extra)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    feed(&mut child, input.as_bytes());
    child.wait_with_output().unwrap()
}

fn transact_file(dir: &Path) -> PathBuf {
    let p = dir.join("w.edn");
    fs::write(&p, "[:assert :x :y 1]").unwrap();
    p
}

#[test]
fn transact_and_repl_write_on_damaged_system_world_name_the_flag_and_leave_the_file() {
    let (dir, _, _) = damaged_system();
    let scratch = tempfile::tempdir().unwrap();
    let edn = transact_file(scratch.path());
    let world = dir.path().join("system.kala");
    let before = tree(dir.path());

    let o = kala(["transact".as_ref(), world.as_os_str(), edn.as_os_str()]);
    assert_ne!(code(&o), 0);
    assert!(
        stderr(&o).contains("--acknowledge-recovery"),
        "{}",
        stderr(&o)
    );
    assert_no_hash(&o);
    assert_eq!(tree(dir.path()), before);

    let o = repl_write(&world, &[], "");
    assert_ne!(code(&o), 0);
    assert!(
        stderr(&o).contains("--acknowledge-recovery"),
        "{}",
        stderr(&o)
    );
    assert_no_hash(&o);
    assert_eq!(tree(dir.path()), before);
}

/// Was `..._refuse_system_world_after_cutting_tail`, which pinned an
/// observed wart: the open cut the damaged tail before the system world was
/// refused. An open no longer cuts a damaged tail (PR #20 review, item B):
/// the flag's cut happens after the refusal, so the refusal is side-effect
/// free.
#[test]
fn transact_and_repl_write_with_acknowledge_recovery_refuse_system_world_without_cutting_tail() {
    for which in ["transact", "repl"] {
        let (dir, damaged, start) = damaged_system();
        let scratch = tempfile::tempdir().unwrap();
        let edn = transact_file(scratch.path());
        let world = dir.path().join("system.kala");
        let o = if which == "transact" {
            kala([
                "transact".as_ref(),
                world.as_os_str(),
                edn.as_os_str(),
                "--acknowledge-recovery".as_ref(),
            ])
        } else {
            repl_write(&world, &["--acknowledge-recovery"], "[:assert :x :y 1]\n")
        };
        assert_ne!(code(&o), 0, "{which}");
        assert!(
            stderr(&o).contains("managed only by `kala user`"),
            "{which}: {}",
            stderr(&o)
        );
        assert_no_hash(&o);
        // Nothing written, nothing cut, nothing quarantined.
        assert!(fs::read(&world).unwrap() == damaged, "{which}");
        let q = dir.path().join(format!("system.kala.torn-{start}"));
        assert!(!q.exists(), "{which}");
    }
}

#[cfg(unix)]
#[test]
fn load_force_over_a_renamed_read_only_system_world_is_refused_and_unchanged() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path().to_str().unwrap();
    ok(&user(&["add", d, "alice@localhost", "--trust"]));
    let renamed = dir.path().join("renamed.kala");
    fs::rename(dir.path().join("system.kala"), &renamed).unwrap();
    fs::set_permissions(&renamed, fs::Permissions::from_mode(0o444)).unwrap();
    let before = tree(dir.path());
    let o = kala([
        "load".as_ref(),
        std::ffi::OsStr::new(VARROW),
        renamed.as_os_str(),
        "--force".as_ref(),
    ]);
    assert_ne!(code(&o), 0, "{}", stdout(&o));
    assert!(
        stderr(&o).contains("managed only by `kala user`"),
        "{}",
        stderr(&o)
    );
    assert_no_hash(&o);
    assert_eq!(tree(dir.path()), before);
}

#[test]
fn recovery_and_write_report_outputs_never_contain_hashes_or_passwords() {
    let (dir, _, _) = damaged_system();
    let d = dir.path().to_str().unwrap();
    for args in [
        vec!["add", d, "bob@localhost", "--trust"],
        vec![
            "add",
            d,
            "bob@localhost",
            "--trust",
            "--acknowledge-recovery",
        ],
        vec!["grant", d, "bob@localhost", "read", "varrow"],
        vec!["set", d, "bob@localhost", "--trust"],
        vec!["drop", d, "bob@localhost"],
    ] {
        assert_no_hash(&user(&args));
    }
}

// ------------------------------------------- Review round: M1 / M2

/// `kala user <args>` with `input` piped to stdin.
fn user_stdin(args: &[&str], input: &str) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_kala"))
        .arg("user")
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    feed(&mut child, input.as_bytes());
    child.wait_with_output().unwrap()
}

const ROTATED: &str = "SECRET-UNIQUE-rotated-pw-555";

/// M1: a password rotation is the damaged last transaction.
#[test]
fn acknowledge_recovery_warns_with_bytes_path_and_not_applied_when_it_cuts_a_damaged_rotation() {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path().to_str().unwrap();
    let o = user_stdin(
        &["add", d, "alice@localhost", "--password-stdin"],
        "first-pw-1234\n",
    );
    ok(&o);
    let o = user_stdin(
        &["set", d, "alice@localhost", "--password-stdin"],
        &format!("{ROTATED}\n"),
    );
    ok(&o);
    let path = dir.path().join("system.kala");
    let mut bytes = fs::read(&path).unwrap();
    let last = damage::spans(&bytes).pop().unwrap();
    bytes[last.body_start + 6] ^= 0x01;
    fs::write(&path, &bytes).unwrap();
    let cut = bytes.len() - last.start;

    let o = user(&[
        "add",
        d,
        "bob@localhost",
        "--trust",
        "--acknowledge-recovery",
    ]);
    ok(&o);
    let q = dir.path().join(format!("system.kala.torn-{}", last.start));
    assert_eq!(fs::metadata(&q).unwrap().len() as usize, cut);
    let err = stderr(&o);
    assert!(
        err.contains(&cut.to_string()),
        "byte count {cut} missing: {err}"
    );
    assert!(err.contains("bytes"), "{err}");
    assert!(
        err.contains(&format!("system.kala.torn-{}", last.start)),
        "quarantine path missing: {err}"
    );
    assert!(err.contains("were not applied"), "{err}");
    for needle in ["$argon2", "password_hash", "SECRET-UNIQUE", "first-pw-1234"] {
        assert!(
            !err.contains(needle) && !stdout(&o).contains(needle),
            "`{needle}` leaked"
        );
    }
}

/// M1: the same warning for a cut damaged grant, from any write subcommand.
#[test]
fn acknowledge_recovery_warns_on_a_damaged_grant_for_every_write_subcommand() {
    // `damaged_system()` leaves only alice after recovery, so every
    // subcommand targets alice, except `add`, which creates bob.
    let subcommands: [(&str, &[&str]); 5] = [
        ("add", &["bob@localhost", "--trust"]),
        ("set", &["alice@localhost", "--trust"]),
        ("grant", &["alice@localhost", "read", "varrow"]),
        ("revoke", &["alice@localhost", "read", "varrow"]),
        ("drop", &["alice@localhost"]),
    ];
    for (action, rest) in subcommands {
        let (dir, _, start) = damaged_system();
        let d = dir.path().to_str().unwrap();
        let mut args = vec![action, d];
        args.extend_from_slice(rest);
        args.push("--acknowledge-recovery");
        let o = user(&args);
        assert_eq!(code(&o), 0, "{action}: {}", stderr(&o));
        let err = stderr(&o);
        assert!(err.contains("were not applied"), "{action}: {err}");
        assert!(
            err.contains(&format!("system.kala.torn-{start}")),
            "{action}: {err}"
        );
    }
}

/// M1 guard: nothing cut, no warning.
#[test]
fn guard_acknowledge_recovery_on_a_clean_world_prints_no_recovery_warning() {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path().to_str().unwrap();
    ok(&user(&["add", d, "alice@localhost", "--trust"]));
    let o = user(&[
        "add",
        d,
        "bob@localhost",
        "--trust",
        "--acknowledge-recovery",
    ]);
    ok(&o);
    let err = stderr(&o);
    assert!(
        !err.contains("were not applied") && !err.contains(".torn-"),
        "{err}"
    );
    assert!(!err.contains("bytes"), "{err}");
}

/// M2.
#[test]
fn user_unknown_subcommand_with_secret_shape_is_redacted() {
    let o = kala(["user", "hunter2-SECRET-789", "/srv", "add"]);
    assert_eq!(code(&o), 2, "{}", stderr(&o));
    let err = stderr(&o);
    assert!(err.contains("unrecognized subcommand"), "{err}");
    assert!(err.contains("<redacted>"), "{err}");
    assert!(
        !err.contains("hunter2") && !err.contains("SECRET-789"),
        "{err}"
    );
    assert!(!stdout(&o).contains("SECRET-789"));
}

#[test]
fn help_user_with_secret_argument_never_echoes_it() {
    for args in [
        vec!["help", "user", "add", "SECRET-UNIQUE-789"],
        vec!["help", "user", "SECRET-UNIQUE-789"],
        vec!["help", "user", "add", "SECRET-UNIQUE-789", "more"],
    ] {
        let o = kala(args.clone());
        assert_ne!(code(&o), 0, "{args:?}");
        let (out, err) = (stdout(&o), stderr(&o));
        assert!(
            !err.contains("SECRET-UNIQUE-789") && !out.contains("SECRET-UNIQUE-789"),
            "{args:?}: {err}"
        );
    }
}

/// M2 guards: real subcommands and `help` keep their names; usage is shown.
#[test]
fn guard_user_redaction_keeps_declared_subcommand_names_and_help() {
    let o = kala(["user", "help", "SECRET-UNIQUE-789"]);
    assert_ne!(code(&o), 0);
    assert!(!stderr(&o).contains("SECRET-UNIQUE-789"), "{}", stderr(&o));
    let o = kala(["user", "grant", "/srv"]);
    assert_eq!(code(&o), 2);
    assert!(stderr(&o).contains("<ACCOUNT>"), "{}", stderr(&o));
    let o = kala(["user", "nonsense"]);
    assert!(
        stderr(&o).contains("Usage: kala user <COMMAND>"),
        "{}",
        stderr(&o)
    );
}

/// M1 edge: the cut warning is also printed when the command then fails.
#[test]
fn acknowledge_recovery_warns_even_when_the_command_fails_after_the_cut() {
    // `set` on an account that does not exist after recovery.
    let (dir, damaged, start) = damaged_system();
    let d = dir.path().to_str().unwrap();
    let o = user(&[
        "set",
        d,
        "bob@localhost",
        "--trust",
        "--acknowledge-recovery",
    ]);
    assert_ne!(code(&o), 0, "{}", stdout(&o));
    let err = stderr(&o);
    assert!(err.contains("were not applied"), "{err}");
    assert!(err.contains(&format!("system.kala.torn-{start}")), "{err}");
    assert_eq!(
        fs::read(dir.path().join(format!("system.kala.torn-{start}"))).unwrap(),
        damaged[start..]
    );

    // `drop` of the last admin: alice is the only admin:*, the damaged tail
    // is a later (read) grant that gets cut.
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path().to_str().unwrap();
    ok(&user(&["add", d, "alice@localhost", "--trust"]));
    ok(&user(&["grant", d, "alice@localhost", "admin", "*"]));
    // The bootstrap account `root@localhost` is also an admin; remove it so
    // alice is the last one.
    ok(&user(&["drop", d, "root@localhost"]));
    ok(&user(&["grant", d, "alice@localhost", "read", "varrow"]));
    let path = dir.path().join("system.kala");
    let mut bytes = fs::read(&path).unwrap();
    let last = damage::spans(&bytes).pop().unwrap();
    bytes[last.body_start + 6] ^= 0x01;
    fs::write(&path, &bytes).unwrap();
    let o = user(&["drop", d, "alice@localhost", "--acknowledge-recovery"]);
    assert_ne!(code(&o), 0, "{}", stdout(&o));
    let err = stderr(&o);
    assert!(err.contains("last account with admin"), "{err}");
    assert!(err.contains("were not applied"), "{err}");
    assert!(
        err.contains(&format!("system.kala.torn-{}", last.start)),
        "{err}"
    );
}

/// M2 edge: `kala help user help <secret>` never echoes the secret.
#[test]
fn help_user_help_with_secret_argument_never_echoes_it() {
    let o = kala(["help", "user", "help", "SECRET-UNIQUE-321"]);
    let (out, err) = (stdout(&o), stderr(&o));
    assert!(
        !err.contains("SECRET-UNIQUE-321") && !out.contains("SECRET-UNIQUE-321"),
        "out={out} err={err}"
    );
}

/// M2 edge: a non-UTF-8 argument in the `user` subcommand position is redacted.
#[cfg(unix)]
#[test]
fn user_non_utf8_subcommand_is_redacted() {
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;
    let bad = OsString::from_vec(b"SECRET-\xff\xfe-654".to_vec());
    let o = kala([OsString::from("user"), bad, OsString::from("/srv")]);
    assert_ne!(code(&o), 0, "{}", stdout(&o));
    let err = stderr(&o);
    assert!(
        !err.contains("SECRET") && !err.contains("654"),
        "secret leaked: {err}"
    );
    assert!(!stdout(&o).contains("SECRET"));
    assert!(err.contains("<redacted>"), "{err}");
}
