//! Slice 6 item 00 (slice-5 follow-ups), RED tests for the `kala` binary.
//!
//! (a) M1: a failed write to stdout/stderr other than BrokenPipe exits 3.
//! (b) An entity read at a `--canon` before it was defined is an error
//!     (exit 1), like an edge; at or after its defining tx it reads.
//!     Reverses the slice-5 rule "reads as empty (exit 0)".
//! (c) L1: the extra "must escape" chars (U+061C, U+200E, U+200F, U+2028,
//!     U+2029, U+FEFF) are shown as `\u{..}` in error echoes and in world
//!     strings.
//! (d) L3: `kala info` on a non-regular file (`/dev/zero`, a FIFO, a
//!     directory) exits 3 quickly, without hanging or allocating without
//!     bound.

mod common;

use common::*;
use kala_core::{AttrDef, Cardinality, Kind, Schema, Tick, ValueType, World};
use serde_json::json;
use std::fs::File;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

/// Runs a command to the end, killing it (and failing) after `secs`.
fn wait_or_kill(mut child: std::process::Child, secs: u64, what: &str) -> std::process::ExitStatus {
    let deadline = Instant::now() + Duration::from_secs(secs);
    loop {
        if let Some(st) = child.try_wait().unwrap() {
            return st;
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("{what}: did not finish within {secs} s (hang)");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

const EXTRA: [(char, &str); 6] = [
    ('\u{061C}', "\\u{61c}"),
    ('\u{200E}', "\\u{200e}"),
    ('\u{200F}', "\\u{200f}"),
    ('\u{2028}', "\\u{2028}"),
    ('\u{2029}', "\\u{2029}"),
    ('\u{FEFF}', "\\u{feff}"),
];

// ---------------------------------------------------------------------------
// (a) M1: output write failures
// ---------------------------------------------------------------------------

fn dev_full() -> Option<File> {
    if !cfg!(target_os = "linux") {
        return None;
    }
    std::fs::OpenOptions::new()
        .write(true)
        .open("/dev/full")
        .ok()
}

#[cfg(target_os = "linux")]
#[test]
fn stdout_to_dev_full_exits_3_json() {
    let Some(full) = dev_full() else {
        eprintln!("skipped: no /dev/full");
        return;
    };
    let fx = varrow();
    let st = Command::new(env!("CARGO_BIN_EXE_kala"))
        .args(["log"])
        .arg(&fx.path)
        .arg("--json")
        .stdout(full)
        .stderr(Stdio::piped())
        .output()
        .unwrap();
    assert_eq!(st.status.code(), Some(3), "stderr: {}", stderr(&st));
}

#[cfg(target_os = "linux")]
#[test]
fn stdout_to_dev_full_exits_3_text_and_other_commands() {
    let Some(_) = dev_full() else {
        eprintln!("skipped: no /dev/full");
        return;
    };
    let fx = varrow();
    for args in [
        vec!["log"],
        vec!["info"],
        vec!["asof", "edwin"],
        vec!["list", "CHARACTER"],
        vec!["doctor"],
    ] {
        let mut full = vec![args[0].to_string(), fx.path.to_str().unwrap().to_string()];
        full.extend(args[1..].iter().map(|s| s.to_string()));
        let o = Command::new(env!("CARGO_BIN_EXE_kala"))
            .args(&full)
            .stdout(dev_full().unwrap())
            .stderr(Stdio::piped())
            .output()
            .unwrap();
        assert_eq!(
            o.status.code(),
            Some(3),
            "kala {args:?} to /dev/full: stderr: {}",
            stderr(&o)
        );
    }
}

#[cfg(target_os = "linux")]
#[test]
fn help_to_dev_full_exits_3() {
    let Some(full) = dev_full() else {
        eprintln!("skipped: no /dev/full");
        return;
    };
    let o = Command::new(env!("CARGO_BIN_EXE_kala"))
        .arg("--help")
        .stdout(full)
        .stderr(Stdio::piped())
        .output()
        .unwrap();
    assert_eq!(o.status.code(), Some(3), "stderr: {}", stderr(&o));
}

#[cfg(target_os = "linux")]
#[test]
fn stderr_to_dev_full_exits_3_even_for_a_user_error() {
    let Some(full) = dev_full() else {
        eprintln!("skipped: no /dev/full");
        return;
    };
    let fx = varrow();
    // `nobody` isn't an entity: exit 1 if the message could be written.
    let o = Command::new(env!("CARGO_BIN_EXE_kala"))
        .args(["asof"])
        .arg(&fx.path)
        .arg("nobody")
        .stdout(Stdio::piped())
        .stderr(full)
        .output()
        .unwrap();
    assert_eq!(o.status.code(), Some(3), "stdout: {}", stdout(&o));
}

#[cfg(target_os = "linux")]
#[test]
fn usage_error_with_stderr_to_dev_full_exits_3() {
    let Some(full) = dev_full() else {
        eprintln!("skipped: no /dev/full");
        return;
    };
    let o = Command::new(env!("CARGO_BIN_EXE_kala"))
        .arg("no-such-command")
        .stdout(Stdio::piped())
        .stderr(full)
        .output()
        .unwrap();
    assert_eq!(o.status.code(), Some(3), "stdout: {}", stdout(&o));
}

#[cfg(target_os = "linux")]
#[test]
fn stdout_to_dev_full_with_an_error_result_keeps_the_error_exit() {
    // The command fails (exit 1) and writes only to stderr; a broken stdout
    // that was never written to must not change the exit code.
    let Some(full) = dev_full() else {
        eprintln!("skipped: no /dev/full");
        return;
    };
    let fx = varrow();
    let o = Command::new(env!("CARGO_BIN_EXE_kala"))
        .args(["asof"])
        .arg(&fx.path)
        .arg("nobody")
        .stdout(full)
        .stderr(Stdio::piped())
        .output()
        .unwrap();
    assert_eq!(o.status.code(), Some(1), "stderr: {}", stderr(&o));
}

/// A string long enough that the output can't fit in a pipe buffer, so the
/// child blocks in `write` until we close the read end: a BrokenPipe that
/// does not depend on timing.
fn big_output_world() -> Fx {
    let long = "x".repeat(400_000);
    build(&format!(
        "schema:\n  node_types: [CHARACTER]\n  attributes:\n    - {{ name: bio, type: string, indexed: false }}\nentities:\n  CHARACTER: [alice]\ntransactions:\n  - tick: 1\n    ops:\n      - assert: [alice, bio, \"{long}\"]\n"
    ))
}

#[cfg(unix)]
#[test]
fn a_closed_stdout_pipe_is_not_exit_3() {
    let fx = big_output_world();
    let mut child = Command::new(env!("CARGO_BIN_EXE_kala"))
        .args(["asof"])
        .arg(&fx.path)
        .arg("alice")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    // Close the read end without reading: the child's write must fail with
    // BrokenPipe (the output is far larger than the pipe buffer).
    drop(child.stdout.take());
    let st = wait_or_kill(child, 30, "kala asof with a closed stdout");
    let code = st.code().expect("must exit, not die of a signal");
    assert_ne!(code, 3, "BrokenPipe is not a store error");
    assert_eq!(code, 0, "a closed pipe is the reader's choice (as before)");
}

// ---------------------------------------------------------------------------
// (b) entity read before it was defined (exit 1), per command
// ---------------------------------------------------------------------------

struct LateFx {
    fx: Fx,
    before: u64,
    def_tx: u64,
}

fn late_fx() -> LateFx {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("w.kala");
    let schema = Schema::new()
        .node_type("CHARACTER")
        .node_type("PLACE")
        .attribute(AttrDef::new(
            "name",
            ValueType::Str,
            Kind::State,
            Cardinality::One,
        ))
        .attribute(AttrDef::new(
            "knows",
            ValueType::Ref,
            Kind::State,
            Cardinality::Many,
        ));
    let (before, def_tx);
    {
        let mut w = World::create(&path, schema).unwrap();
        let alice = w.create_entity("CHARACTER", Some("alice")).unwrap();
        w.assert(
            alice,
            "name",
            kala_core::Value::Str("Alice".into()),
            Some(Tick(1)),
        )
        .unwrap();
        before = w.latest_tx().0;
        let late = w.create_entity("CHARACTER", Some("late")).unwrap();
        def_tx = w.latest_tx().0;
        w.assert(
            late,
            "name",
            kala_core::Value::Str("Late".into()),
            Some(Tick(2)),
        )
        .unwrap();
        w.assert(late, "knows", kala_core::Value::Ref(alice), Some(Tick(2)))
            .unwrap();
    }
    assert!(def_tx > before);
    let mem = World::open_read_only(&path).unwrap();
    LateFx {
        fx: Fx { dir, path, mem },
        before,
        def_tx,
    }
}

#[test]
fn asof_history_neighbors_before_the_entity_exists_exit_1() {
    let f = late_fx();
    let c = f.before.to_string();
    let tagged = format!("tx#{}", f.before);
    for canon in [c.as_str(), tagged.as_str(), "0"] {
        let err = f.fx.fails(1, "asof", &["late", "--canon", canon]);
        assert!(err.to_lowercase().contains("entity"), "{err}");
        f.fx.fails(1, "asof", &["late", "name", "@2", "--canon", canon]);
        f.fx.fails(1, "history", &["late", "name", "--canon", canon]);
        f.fx.fails(1, "neighbors", &["late", "knows", "--canon", canon]);
        f.fx.fails(
            1,
            "neighbors",
            &["late", "knows", "--both", "--canon", canon],
        );
    }
}

#[test]
fn the_entity_by_id_form_is_refused_too_before_it_exists() {
    let f = late_fx();
    let late = f.fx.id("late");
    let id = f.fx.mem.format_id(late);
    let c = f.before.to_string();
    f.fx.fails(1, "asof", &[&id, "--canon", &c]);
    f.fx.fails(1, "history", &[&id, "name", "--canon", &c]);
}

#[test]
fn at_the_defining_tx_the_entity_reads_with_nothing_on_it() {
    let f = late_fx();
    let c = f.def_tx.to_string();
    let j = f.fx.json("asof", &["late", "--canon", &c]);
    assert_eq!(j["canon"], json!(f.def_tx));
    assert_eq!(j["attributes"], json!([]));
    let h = f.fx.json("history", &["late", "name", "--canon", &c]);
    assert_eq!(h["changes"], json!([]));
    let n = f.fx.json("neighbors", &["late", "knows", "--canon", &c]);
    assert_eq!(n["neighbors"], json!([]));
}

#[test]
fn after_the_defining_tx_and_at_latest_the_entity_reads_normally() {
    let f = late_fx();
    let latest = f.fx.mem.latest_tx().0.to_string();
    let j = f.fx.json("asof", &["late", "--canon", &latest]);
    assert!(!j["attributes"].as_array().unwrap().is_empty());
    let j = f.fx.json("asof", &["late"]);
    assert!(!j["attributes"].as_array().unwrap().is_empty());
    // Beyond the latest canon behaves as the latest.
    f.fx.json("asof", &["late", "--canon", "999999"]);
}

#[test]
fn an_entity_defined_before_the_canon_still_reads_at_that_canon() {
    let f = late_fx();
    let c = f.before.to_string();
    let j = f.fx.json("asof", &["alice", "--canon", &c]);
    assert_eq!(j["canon"], json!(f.before));
    assert!(!j["attributes"].as_array().unwrap().is_empty());
}

#[test]
fn list_at_a_canon_before_the_entity_simply_omits_it() {
    let f = late_fx();
    let c = f.before.to_string();
    let l = f.fx.json("list", &["CHARACTER", "--canon", &c]);
    let handles: Vec<_> = l["entities"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["handle"].clone())
        .collect();
    assert_eq!(handles, vec![json!("alice")]);
}

// ---------------------------------------------------------------------------
// (c) L1: the extra escaped chars, in error echoes and world strings
// ---------------------------------------------------------------------------

#[test]
fn an_argument_echoed_in_an_error_escapes_the_extra_chars() {
    let fx = varrow();
    for (c, want) in EXTRA {
        let arg = format!("x{c}y");
        let o = fx.run("asof", &[arg.as_str()]);
        assert_eq!(code(&o), 1, "{c:?}: {}", stderr(&o));
        let err = stderr(&o);
        assert!(!err.contains(c), "raw U+{:04X} in {err:?}", u32::from(c));
        assert!(err.contains(want), "{want} missing in {err:?}");
    }
}

#[test]
fn a_usage_error_echoing_an_argument_escapes_the_extra_chars() {
    for (c, want) in EXTRA {
        let arg = format!("bad{c}cmd");
        let o = kala([arg.as_str()]);
        assert_eq!(code(&o), 2, "{c:?}: {}", stderr(&o));
        let err = stderr(&o);
        assert!(!err.contains(c), "raw U+{:04X} in {err:?}", u32::from(c));
        assert!(err.contains(want), "{want} missing in {err:?}");
    }
}

fn strings_world() -> Fx {
    // Names holding each extra char, written as YAML `\u` escapes.
    let names: Vec<String> = EXTRA
        .iter()
        .map(|(c, _)| format!("n{:04X}", u32::from(*c)))
        .collect();
    let mut ops = String::new();
    for ((c, _), h) in EXTRA.iter().zip(&names) {
        ops += &format!(
            "      - assert: [{h}, name, \"a\\u{:04X}b\"]\n",
            u32::from(*c)
        );
    }
    build(&format!(
        "schema:\n  node_types: [CHARACTER]\n  attributes:\n    - {{ name: name, type: string }}\nentities:\n  CHARACTER: [{}]\ntransactions:\n  - tick: 1\n    ops:\n{ops}",
        names.join(", ")
    ))
}

#[test]
fn a_world_string_with_the_extra_chars_is_escaped_in_text_output() {
    let fx = strings_world();
    for ((c, want), h) in EXTRA
        .iter()
        .zip(EXTRA.iter().map(|(c, _)| format!("n{:04X}", u32::from(*c))))
    {
        let out = fx.text("asof", &[h.as_str()]);
        assert!(!out.contains(*c), "raw U+{:04X} in {out:?}", u32::from(*c));
        assert!(out.contains(want), "{want} missing in {out:?}");
        no_raw_controls(&out);
    }
}

#[test]
fn a_world_string_with_the_extra_chars_is_not_raw_in_json_and_round_trips() {
    let fx = strings_world();
    for (c, _) in EXTRA {
        let h = format!("n{:04X}", u32::from(c));
        let o = fx.run("asof", &[h.as_str(), "--json"]);
        assert_eq!(code(&o), 0, "{}", stderr(&o));
        let raw = stdout(&o);
        assert!(
            !raw.contains(c),
            "raw U+{:04X} in JSON {raw:?}",
            u32::from(c)
        );
        let j = parse_one_json(&raw);
        let want = format!("a{c}b");
        let shown = j["attributes"][0]["values"][0]["value"]["value"].clone();
        assert_eq!(shown, json!(want), "JSON value must round-trip exactly");
    }
}

// ---------------------------------------------------------------------------
// (d) L3: `kala info` on non-regular files
// ---------------------------------------------------------------------------

/// `kala info <path>` under a 3 GB address-space cap, killed after 30 s.
fn info_capped(path: &Path) -> (Option<i32>, String) {
    let mut child = Command::new("sh")
        .arg("-c")
        .arg("ulimit -v 3000000; exec \"$0\" info \"$1\"")
        .arg(env!("CARGO_BIN_EXE_kala"))
        .arg(path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stderr_handle = child.stderr.take().unwrap();
    let reader = std::thread::spawn(move || {
        use std::io::Read;
        let mut s = String::new();
        let _ = stderr_handle.read_to_string(&mut s);
        s
    });
    let st = wait_or_kill(child, 30, &format!("kala info {}", path.display()));
    (st.code(), reader.join().unwrap())
}

#[test]
fn info_on_dev_zero_exits_3_quickly() {
    if !Path::new("/dev/zero").exists() {
        eprintln!("skipped: no /dev/zero");
        return;
    }
    let started = Instant::now();
    let (code, err) = info_capped(Path::new("/dev/zero"));
    assert_eq!(code, Some(3), "stderr: {err}");
    assert!(err.starts_with("kala: error: "), "{err}");
    // The 3 GB cap turns an unbounded read into an allocation failure; that
    // must not be what produced the exit 3: the file must be refused first.
    let lower = err.to_lowercase();
    assert!(
        !lower.contains("out of memory") && !lower.contains("outofmemory"),
        "refused only by running out of memory: {err}"
    );
    assert!(started.elapsed() < Duration::from_secs(20));
}

#[test]
fn info_on_a_fifo_exits_3_without_hanging() {
    let dir = tempfile::tempdir().unwrap();
    let fifo = dir.path().join("pipe");
    match Command::new("mkfifo").arg(&fifo).status() {
        Ok(st) if st.success() => {}
        _ => {
            eprintln!("skipped: mkfifo unavailable");
            return;
        }
    }
    let (code, err) = info_capped(&fifo);
    assert_eq!(code, Some(3), "stderr: {err}");
}

#[test]
fn info_on_a_directory_exits_3() {
    let dir = tempfile::tempdir().unwrap();
    let (code, err) = info_capped(dir.path());
    assert_eq!(code, Some(3), "stderr: {err}");
}

#[test]
fn info_on_a_missing_file_is_still_exit_1() {
    let dir = tempfile::tempdir().unwrap();
    let (code, err) = info_capped(&dir.path().join("nope.kala"));
    assert_eq!(code, Some(1), "stderr: {err}");
}

#[test]
fn info_on_a_regular_world_still_works() {
    let fx = varrow();
    let o = fx.run("info", &[]);
    assert_eq!(code(&o), 0, "{}", stderr(&o));
}
