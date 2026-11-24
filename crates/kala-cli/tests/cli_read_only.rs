//! Slice 5 follow-up: the read commands never take the world's lock and
//! never change the file (RED step; spec: CLAUDE.md "One writer at a time,
//! many readers", PM decision: kala-core gets `World::open_read_only` and the
//! CLI's reads use it).
//!
//!  * every read command (asof, neighbors, history, find, list, edge, log,
//!    info, doctor) succeeds while the test process holds the world open with
//!    `World::open` (lock held), and answers as it does with no lock held;
//!  * on a world with a torn or damaged tail, the read commands leave the
//!    file bytes and the directory listing unchanged (no `.torn-*`
//!    quarantine) and answer from the valid committed prefix;
//!  * `info` and `doctor` don't modify the file either.

mod common;
#[path = "common/damage.rs"]
mod damage;

use common::*;
use damage::*;
use kala_core::World;
use serde_json::json;
use std::ffi::OsStr;
use std::path::Path;

/// `(command, args after the world path)`. Every read command of the CLI.
const READS: &[(&str, &[&str])] = &[
    ("asof", &["edwin"]),
    ("asof", &["edwin", "name", "@-200"]),
    ("neighbors", &["edwin", "ally_of"]),
    ("history", &["edwin", "name"]),
    ("find", &["name", "Edwin"]),
    ("find", &["name", "edwin", "-i"]),
    ("list", &["CHARACTER"]),
    ("edge", &["edge#1"]),
    ("edge", &["edwin", "ally_of", "mara"]),
    ("log", &[]),
    ("info", &[]),
    ("doctor", &[]),
];

fn run_in(path: &Path, cmd: &str, rest: &[&str], json: bool) -> std::process::Output {
    let mut args: Vec<&OsStr> = vec![OsStr::new(cmd), path.as_os_str()];
    args.extend(rest.iter().map(|s| OsStr::new(*s)));
    if json {
        args.push(OsStr::new("--json"));
    }
    kala(args)
}

fn ok_stdout(path: &Path, cmd: &str, rest: &[&str], json: bool) -> String {
    let o = run_in(path, cmd, rest, json);
    assert_eq!(
        code(&o),
        0,
        "kala {cmd} {rest:?} json={json}: stderr: {}",
        stderr(&o)
    );
    stdout(&o)
}

// ------------------------------------------------------------ lock held

#[test]
fn every_read_command_succeeds_while_the_world_is_open_elsewhere() {
    let fx = varrow();
    // answers with no lock held, for comparison
    let free: Vec<(String, String)> = READS
        .iter()
        .map(|(c, r)| {
            (
                ok_stdout(&fx.path, c, r, true),
                ok_stdout(&fx.path, c, r, false),
            )
        })
        .collect();
    let before = std::fs::read(&fx.path).unwrap();
    let dir_before = dir_snapshot(fx.dir.path());

    // Slice 15b: an open writer world holds the lock only while it commits,
    // so the lock is also held explicitly, as a commit in progress does.
    let held = World::open(&fx.path).expect("a writer world, open elsewhere");
    let lock = std::fs::File::open(&fx.path).unwrap();
    lock.try_lock().expect("test process takes the lock");
    assert_eq!(
        World::open_waiting(&fx.path, std::time::Duration::ZERO).unwrap_err(),
        kala_core::Error::WorldLocked,
        "precondition: the lock is held"
    );
    for ((c, r), (free_json, free_text)) in READS.iter().zip(&free) {
        let o = run_in(&fx.path, c, r, true);
        assert_eq!(
            code(&o),
            0,
            "kala {c} {r:?} --json with the lock held: stderr: {}",
            stderr(&o)
        );
        assert!(!stderr(&o).contains("locked"), "{}", stderr(&o));
        assert_eq!(
            &stdout(&o),
            free_json,
            "kala {c} {r:?}: json answer differs"
        );
        if *c != "info" {
            // info's text says whether recovery ran; its json is compared above
            assert_eq!(
                &ok_stdout(&fx.path, c, r, false),
                free_text,
                "kala {c} {r:?} text"
            );
        } else {
            ok_stdout(&fx.path, c, r, false);
        }
    }
    drop(lock);
    drop(held);
    assert_eq!(std::fs::read(&fx.path).unwrap(), before);
    assert_eq!(dir_snapshot(fx.dir.path()), dir_before);
}

#[test]
fn reads_see_what_the_open_writer_has_committed() {
    let fx = varrow();
    let mut held = World::open(&fx.path).unwrap();
    let edwin = held.entity_by_handle("edwin").unwrap();
    held.assert(
        edwin,
        "alias",
        kala_core::Value::Str("The Grey King".into()),
        Some(kala_core::Tick(9000)),
    )
    .unwrap();
    let tx = held.latest_tx().0;
    let j = fx.json("info", &[]);
    assert_eq!(j["latest_tx"], json!(tx));
    assert_eq!(j["latest_tick"], json!(9000));
    let j = fx.json("find", &["alias", "The Grey King", "@9000"]);
    let found = j["entities"].as_array().expect("entities");
    assert_eq!(found.len(), 1, "{j}");
    assert_eq!(found[0]["label"], json!("edwin"));
}

// ------------------------------------------------------------ torn and damaged tails

/// A tail-damaged copy of varrow, and a clean file holding exactly its valid
/// committed prefix (in its own directory, also named `w.kala`).
struct Cut {
    dir: tempfile::TempDir,
    path: std::path::PathBuf,
    bytes: Vec<u8>,
    keep: usize,
    prefix_dir: tempfile::TempDir,
    prefix: std::path::PathBuf,
}

fn cut_world(damage: impl Fn(&mut Vec<u8>, &Span)) -> Cut {
    let fx = varrow();
    let mut b = std::fs::read(&fx.path).unwrap();
    let last = *spans(&b).last().unwrap();
    let prefix_bytes = b[..last.start].to_vec();
    damage(&mut b, &last);
    let (dir, path) = file_with(&b);
    let (prefix_dir, prefix) = file_with(&prefix_bytes);
    Cut {
        dir,
        path,
        bytes: b,
        keep: last.start,
        prefix_dir,
        prefix,
    }
}

fn torn(into: usize) -> Cut {
    cut_world(|b, last| b.truncate(last.start + into))
}

fn damaged() -> Cut {
    cut_world(|b, last| b[last.body_start + 6] ^= 0x01)
}

const PURE_READS: &[&str] = &[
    "asof",
    "neighbors",
    "history",
    "find",
    "list",
    "edge",
    "log",
];

fn check_reads_leave_the_cut_alone(c: &Cut, what: &str) {
    let dir_before = dir_snapshot(c.dir.path());
    for (cmd, rest) in READS.iter().filter(|(c, _)| PURE_READS.contains(c)) {
        let want = ok_stdout(&c.prefix, cmd, rest, true);
        let got = ok_stdout(&c.path, cmd, rest, true);
        assert_eq!(
            got, want,
            "{what}: kala {cmd} {rest:?} must answer from the valid prefix"
        );
        let want_t = ok_stdout(&c.prefix, cmd, rest, false);
        let got_t = ok_stdout(&c.path, cmd, rest, false);
        assert_eq!(got_t, want_t, "{what}: kala {cmd} {rest:?} (text)");
        assert_eq!(
            std::fs::read(&c.path).unwrap(),
            c.bytes,
            "{what}: kala {cmd} {rest:?} modified the file"
        );
        assert_eq!(
            dir_snapshot(c.dir.path()),
            dir_before,
            "{what}: kala {cmd} {rest:?} changed the directory (quarantine?)"
        );
    }
    assert_eq!(
        std::fs::read(&c.prefix).unwrap(),
        c.bytes[..c.keep],
        "the prefix twin is untouched"
    );
    let _ = &c.prefix_dir;
}

#[test]
fn reads_on_a_torn_tail_answer_from_the_prefix_and_change_nothing() {
    for into in [0usize, 10, 24, 25, 40, 200] {
        check_reads_leave_the_cut_alone(&torn(into), &format!("torn into={into}"));
    }
}

#[test]
fn reads_on_a_damaged_tail_answer_from_the_prefix_and_change_nothing() {
    check_reads_leave_the_cut_alone(&damaged(), "damaged");
}

#[test]
fn reads_on_a_torn_tail_still_work_while_another_reader_has_it_open_readonly() {
    // the CLI itself is two processes reading at once: run two in a row on
    // the same cut file and check the file is still the cut file
    let c = torn(40);
    let a = ok_stdout(&c.path, "asof", &["edwin"], true);
    let b = ok_stdout(&c.path, "asof", &["edwin"], true);
    assert_eq!(a, b);
    assert_eq!(std::fs::read(&c.path).unwrap(), c.bytes);
}

#[test]
fn doctor_on_a_cut_tail_changes_nothing() {
    for c in [torn(40), damaged()] {
        let dir_before = dir_snapshot(c.dir.path());
        let o = run_in(&c.path, "doctor", &[], true);
        assert_eq!(
            code(&o),
            1,
            "doctor reports the incomplete tail as an error: {}",
            stderr(&o)
        );
        assert_eq!(std::fs::read(&c.path).unwrap(), c.bytes);
        assert_eq!(dir_snapshot(c.dir.path()), dir_before);
    }
}

// ------------------------------------------------------------ info is read-only too

#[test]
fn info_on_a_torn_tail_reports_what_would_be_cut_and_changes_nothing() {
    let c = torn(40);
    let dir_before = dir_snapshot(c.dir.path());
    let j = parse_one_json(&ok_stdout(&c.path, "info", &[], true));
    assert_eq!(
        j["recovery"],
        json!({"bytes_cut": c.bytes.len() - c.keep, "cause": "torn", "quarantine": null})
    );
    // the counts and latest tx are those of the valid prefix
    let (_d, copy) = file_with(&c.bytes);
    let w = World::open(&copy).unwrap();
    assert_eq!(j["latest_tx"], json!(w.latest_tx().0));
    assert_eq!(j["latest_tick"], json!(w.latest_tick().0));
    assert_eq!(std::fs::read(&c.path).unwrap(), c.bytes, "file unchanged");
    assert_eq!(dir_snapshot(c.dir.path()), dir_before, "no quarantine file");
}

#[test]
fn info_text_on_a_torn_tail_says_it_would_be_cut_when_opened_for_writing() {
    let c = torn(40);
    let out = ok_stdout(&c.path, "info", &[], false);
    let low = out.to_lowercase();
    assert!(low.contains("torn"), "{out}");
    assert!(out.contains(&(c.bytes.len() - c.keep).to_string()), "{out}");
    assert!(
        low.contains("would"),
        "text must say the tail would be cut: {out}"
    );
    assert!(
        low.contains("writing"),
        "...when next opened for writing: {out}"
    );
    assert!(
        !out.contains(".torn-"),
        "no quarantine path (none was written): {out}"
    );
    assert!(!low.contains("saved to"), "{out}");
    assert!(!out.contains("acknowledge"), "no hint on a torn cut: {out}");
    assert_eq!(std::fs::read(&c.path).unwrap(), c.bytes);
}

#[test]
fn info_on_a_damaged_tail_reports_it_keeps_the_acknowledge_hint_and_changes_nothing() {
    let c = damaged();
    let dir_before = dir_snapshot(c.dir.path());
    let j = parse_one_json(&ok_stdout(&c.path, "info", &[], true));
    assert_eq!(
        j["recovery"],
        json!({"bytes_cut": c.bytes.len() - c.keep, "cause": "damaged", "quarantine": null})
    );
    let out = ok_stdout(&c.path, "info", &[], false);
    assert!(out.to_lowercase().contains("damaged"), "{out}");
    assert!(out.contains("acknowledge"), "{out}");
    assert!(out.to_lowercase().contains("would"), "{out}");
    assert!(!out.contains(".torn-"), "{out}");
    assert_eq!(std::fs::read(&c.path).unwrap(), c.bytes);
    assert_eq!(dir_snapshot(c.dir.path()), dir_before);
}

#[test]
fn info_on_a_clean_world_says_nothing_would_be_cut() {
    let fx = varrow();
    let out = fx.text("info", &[]);
    assert!(!out.to_lowercase().contains("would"), "{out}");
    assert_eq!(
        fx.json("info", &[])["recovery"],
        json!({"bytes_cut": 0, "cause": null, "quarantine": null})
    );
}

// ------------------------------------------------------------ errors keep their exit codes

#[test]
fn a_missing_world_is_exit_1_and_not_created_for_every_read_command() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("nope.kala");
    for (cmd, rest) in READS {
        let o = run_in(&p, cmd, rest, false);
        assert_eq!(code(&o), 1, "{cmd}: {}", stderr(&o));
        assert!(stderr(&o).starts_with("kala: error: "), "{}", stderr(&o));
    }
    assert!(dir_names(dir.path()).is_empty(), "nothing created");
}

#[test]
fn mid_log_damage_is_exit_3_and_untouched_for_every_read_command() {
    let fx = varrow();
    let mut b = std::fs::read(&fx.path).unwrap();
    let mid = spans(&b).iter().find(|s| s.tx == 3).copied().unwrap();
    b[mid.body_start + 6] ^= 0x01;
    let (d, p) = file_with(&b);
    let dir_before = dir_snapshot(d.path());
    for (cmd, rest) in READS.iter().filter(|(c, _)| *c != "doctor") {
        let o = run_in(&p, cmd, rest, false);
        assert_eq!(code(&o), 3, "{cmd}: {}", stderr(&o));
        assert!(stderr(&o).starts_with("kala: error: "), "{}", stderr(&o));
    }
    assert_eq!(std::fs::read(&p).unwrap(), b);
    assert_eq!(dir_snapshot(d.path()), dir_before);
}
