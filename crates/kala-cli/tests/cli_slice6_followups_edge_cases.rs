//! Slice 6 item 00: CLI edge cases found while confirming.

mod common;

use common::*;
use kala_core::{AttrDef, Cardinality, Kind, Schema, Tick, ValueType, World};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

fn run_capped(mut cmd: Command, secs: u64) -> (Option<i32>, String) {
    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut h = child.stderr.take().unwrap();
    let reader = std::thread::spawn(move || {
        use std::io::Read;
        let mut s = String::new();
        let _ = h.read_to_string(&mut s);
        s
    });
    let deadline = Instant::now() + Duration::from_secs(secs);
    let st = loop {
        if let Some(st) = child.try_wait().unwrap() {
            break st;
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("did not finish within {secs} s (hang)");
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    (st.code(), reader.join().unwrap())
}

fn mkfifo(path: &std::path::Path) -> bool {
    matches!(Command::new("mkfifo").arg(path).status(), Ok(s) if s.success())
}

#[test]
fn edge_read_as_a_subject_before_its_define_exits_1() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("w.kala");
    let schema = Schema::new()
        .node_type("CHARACTER")
        .node_type("PLACE")
        .attribute(AttrDef::new(
            "present",
            ValueType::Ref,
            Kind::State,
            Cardinality::Many,
        ))
        .attribute(AttrDef::new(
            "disguised",
            ValueType::Bool,
            Kind::State,
            Cardinality::One,
        ));
    let before;
    {
        let mut w = World::create(&path, schema).unwrap();
        let a = w.create_entity("CHARACTER", Some("a")).unwrap();
        let p = w.create_entity("PLACE", Some("p")).unwrap();
        before = w.latest_tx().0;
        w.assert(a, "present", kala_core::Value::Ref(p), Some(Tick(1)))
            .unwrap();
        let e: kala_core::EntityId = w.edge_id(a, "present", p).unwrap().into();
        w.assert(e, "disguised", kala_core::Value::Bool(true), Some(Tick(1)))
            .unwrap();
    }
    let mem = World::open_read_only(&path).unwrap();
    let fx = Fx { dir, path, mem };
    let c = before.to_string();
    // sanity: reads at latest
    fx.json("asof", &["edge#1"]);
    let err = fx.fails(1, "asof", &["edge#1", "--canon", &c]);
    assert!(
        err.to_lowercase().contains("edge") || err.to_lowercase().contains("entity"),
        "{err}"
    );
    fx.fails(1, "history", &["edge#1", "disguised", "--canon", &c]);
}

#[cfg(target_os = "linux")]
#[test]
fn a_stdout_failure_also_writes_one_error_line_to_stderr() {
    let Ok(full) = std::fs::OpenOptions::new().write(true).open("/dev/full") else {
        eprintln!("skipped: no /dev/full");
        return;
    };
    let fx = varrow();
    let o = Command::new(env!("CARGO_BIN_EXE_kala"))
        .args(["info"])
        .arg(&fx.path)
        .stdout(full)
        .stderr(Stdio::piped())
        .output()
        .unwrap();
    assert_eq!(code(&o), 3);
    let err = stderr(&o);
    assert_eq!(err.lines().count(), 1, "{err:?}");
    assert!(err.starts_with("kala: error: writing output:"), "{err:?}");
    assert!(err.ends_with('\n'));
    no_raw_controls(&err);
}

#[test]
fn doctor_on_a_fifo_reports_promptly_and_does_not_hang() {
    let dir = tempfile::tempdir().unwrap();
    let fifo = dir.path().join("pipe");
    if !mkfifo(&fifo) {
        eprintln!("skipped: mkfifo unavailable");
        return;
    }
    let mut c = Command::new(env!("CARGO_BIN_EXE_kala"));
    c.arg("doctor").arg(&fifo);
    let (code, err) = run_capped(c, 20);
    // pinned: refused like `info`, as a store error
    assert_eq!(code, Some(3), "stderr: {err}");
    assert!(err.starts_with("kala: error: "), "{err}");
}

#[cfg(unix)]
#[test]
fn a_symlink_to_a_fifo_is_refused_without_hanging() {
    let dir = tempfile::tempdir().unwrap();
    let fifo = dir.path().join("pipe");
    if !mkfifo(&fifo) {
        eprintln!("skipped: mkfifo unavailable");
        return;
    }
    let link = dir.path().join("link");
    std::os::unix::fs::symlink(&fifo, &link).unwrap();
    for cmd in ["info", "doctor", "log"] {
        let mut c = Command::new(env!("CARGO_BIN_EXE_kala"));
        c.arg(cmd).arg(&link);
        let (code, err) = run_capped(c, 20);
        assert_eq!(code, Some(3), "kala {cmd}: stderr: {err}");
    }
}

#[test]
fn a_file_that_grows_after_open_is_read_as_a_snapshot_or_prefix() {
    // Deterministic form: append garbage after the last commit on a copy and
    // check the reader treats it as a torn tail, not as a hang or a crash;
    // concurrent growth itself is not deterministic enough to test.
    let fx = varrow();
    let o = fx.run("info", &[]);
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    use std::io::Write;
    let mut f = std::fs::OpenOptions::new()
        .append(true)
        .open(&fx.path)
        .unwrap();
    f.write_all(&[0xAB; 7]).unwrap();
    drop(f);
    let mut c = Command::new(env!("CARGO_BIN_EXE_kala"));
    c.arg("info").arg(&fx.path);
    let (code, err) = run_capped(c, 20);
    assert!(
        matches!(code, Some(0) | Some(1) | Some(3)),
        "{code:?} {err}"
    );
}
