mod common;
use common::*;
use std::io::Write;
use std::process::{Command, Output, Stdio};

const QUERY: &str = "[:find ?a :where [:edwin :ally_of ?a]]";

fn repl(fx: &Fx, input: &[u8], flags: &[&str]) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_kala"))
        .arg("repl")
        .arg(&fx.path)
        .args(flags)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    feed(&mut child, input);
    child.wait_with_output().unwrap()
}

#[test]
fn extreme_ticks_and_canon_zero_and_future_are_supported() {
    let fx = varrow();
    for tick in [i64::MIN, -210, 0, i64::MAX] {
        let o = repl(&fx, format!(".at {tick}\n{QUERY}\n").as_bytes(), &[]);
        assert_eq!(code(&o), 0, "tick {tick}: {}", stderr(&o));
        assert!(stdout(&o).starts_with(&format!("query at tick {tick},")));
    }
    let o = repl(
        &fx,
        format!(".canon tx#0\n{QUERY}\n.canon 999\n{QUERY}\n").as_bytes(),
        &[],
    );
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    assert!(stdout(&o).contains("canon tx#0: 0 rows"));
    assert!(stdout(&o).contains("canon tx#12:"));
}

#[test]
fn oversized_input_is_discarded_and_later_query_runs() {
    let fx = varrow();
    let input = format!("[{}\n{QUERY}\n", " ".repeat(kala_datalog::MAX_QUERY_BYTES));
    let o = repl(&fx, input.as_bytes(), &[]);
    assert_eq!(code(&o), 1);
    assert!(
        stderr(&o).contains(&kala_datalog::MAX_QUERY_BYTES.to_string()),
        "{}",
        stderr(&o)
    );
    assert_eq!(stdout(&o), fx.text("query", &[QUERY]));
}

#[test]
fn invalid_utf8_is_reported_and_later_line_runs() {
    let fx = varrow();
    let mut input = b"\xff\xfe\n".to_vec();
    input.extend_from_slice(format!("{QUERY}\n").as_bytes());
    let o = repl(&fx, &input, &[]);
    assert_eq!(code(&o), 1);
    assert!(stderr(&o).contains("UTF-8"), "{}", stderr(&o));
    assert_eq!(stdout(&o), fx.text("query", &[QUERY]));
}

#[test]
fn nul_and_dot_alone_are_errors_with_no_raw_controls() {
    let fx = varrow();
    let o = repl(&fx, format!("\0\n.\n{QUERY}\n").as_bytes(), &[]);
    assert_eq!(code(&o), 1);
    assert!(!stderr(&o).contains('\0'));
    assert_eq!(
        stderr(&o)
            .lines()
            .filter(|l| l.starts_with("error:"))
            .count(),
        2
    );
    assert_eq!(stdout(&o), fx.text("query", &[QUERY]));
}

#[test]
fn crlf_trailing_spaces_and_blank_lines_work() {
    let fx = varrow();
    let o = repl(
        &fx,
        format!("\r\n.at -210   \r\n{QUERY}\r\n.quit   \r\n").as_bytes(),
        &[],
    );
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    assert!(stdout(&o).starts_with("query at tick -210,"));
}

#[test]
fn incomplete_eof_reports_error_and_meta_commands_stay_single_line() {
    let fx = varrow();
    let o = repl(&fx, b"[:find ?a\n", &[]);
    assert_eq!(code(&o), 1);
    assert!(stderr(&o).contains("error:"));
    let o = repl(&fx, format!(".at nope\n{QUERY}\n").as_bytes(), &[]);
    assert_eq!(code(&o), 1);
    assert_eq!(stdout(&o), fx.text("query", &[QUERY]));
}

#[cfg(unix)]
#[test]
fn explicit_history_is_private_and_only_written_when_opted_in() {
    use std::os::unix::fs::PermissionsExt;
    let fx = varrow();
    let path = fx.dir.path().join("history");
    let o = repl(
        &fx,
        format!("{QUERY}\n.quit\n").as_bytes(),
        &["--history", path.to_str().unwrap()],
    );
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    let metadata = std::fs::metadata(&path).unwrap();
    assert_eq!(metadata.permissions().mode() & 0o777, 0o600);
    assert!(std::fs::read_to_string(&path).unwrap().contains(QUERY));
}

#[cfg(unix)]
#[test]
fn history_refuses_symlinks_hardlinks_world_and_special_files() {
    use std::os::unix::fs::symlink;
    let fx = varrow();
    let target = fx.dir.path().join("target");
    std::fs::write(&target, "preserve me\n").unwrap();
    let symlink_path = fx.dir.path().join("symlink");
    symlink(&target, &symlink_path).unwrap();
    let hardlink_path = fx.dir.path().join("hardlink");
    std::fs::hard_link(&target, &hardlink_path).unwrap();
    let original_world = std::fs::read(&fx.path).unwrap();
    for path in [&symlink_path, &hardlink_path, &fx.path, fx.dir.path()] {
        let o = repl(&fx, b".quit\n", &["--history", path.to_str().unwrap()]);
        assert_ne!(code(&o), 0, "history accepted {}", path.display());
        assert!(stderr(&o).contains("error:"), "{}", stderr(&o));
    }
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "preserve me\n");
    assert_eq!(std::fs::read(&fx.path).unwrap(), original_world);
}

#[cfg(unix)]
#[test]
fn unsafe_history_entries_are_rejected_and_never_persisted() {
    let fx = varrow();
    let history = fx.dir.path().join("history");
    for text in [".help\u{1b}]0;forged title\u{7}", ".help\u{202e}spoof"] {
        let source = format!("{}\n", serde_json::to_string(text).unwrap());
        std::fs::write(&history, &source).unwrap();
        let o = repl(&fx, b".quit\n", &["--history", history.to_str().unwrap()]);
        assert_ne!(code(&o), 0, "untrusted history control accepted");
        no_raw_controls(&stderr(&o));
        assert!(!stderr(&o).contains('\u{202e}'));
        assert_eq!(std::fs::read_to_string(&history).unwrap(), source);
    }
    std::fs::write(&history, "").unwrap();
    let o = repl(
        &fx,
        format!(".bad\u{1b}]0;x\u{7}\n.bad\u{202e}\n{QUERY}\n.quit\n").as_bytes(),
        &["--history", history.to_str().unwrap()],
    );
    assert_eq!(code(&o), 1);
    let saved = std::fs::read_to_string(&history).unwrap();
    for line in saved.lines() {
        let value: String = serde_json::from_str(line).unwrap();
        assert!(
            !value.contains('\u{1b}') && !value.contains('\u{7}') && !value.contains('\u{202e}'),
            "unsafe saved entry {value:?}"
        );
    }
    assert!(saved.contains(QUERY));
}

#[test]
fn rule_and_explain_accept_empty_initial_arguments() {
    let fx = varrow();
    let o = repl(&fx, b".rule\n[[(r ?e) [?e :name _]]]\n.explain\n[:find ?e :where (r ?e)]\n[:find ?e :where (r ?e)]\n", &[]);
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    assert!(stdout(&o).contains("Find ?e"));
    assert!(stdout(&o).contains("query at tick"));
}

#[test]
fn missing_multiline_command_body_at_eof_is_an_error() {
    let fx = varrow();
    for command in [".rule\n", ".explain\n"] {
        let output = repl(&fx, command.as_bytes(), &[]);
        assert_eq!(
            code(&output),
            1,
            "unfinished {command:?}: {}",
            stderr(&output)
        );
        assert!(stderr(&output).contains("error:"));
        assert!(output.stdout.is_empty());
    }
}

#[cfg(target_os = "linux")]
#[test]
fn failed_stdout_write_exits_three() {
    let fx = varrow();
    let output = std::fs::OpenOptions::new()
        .write(true)
        .open("/dev/full")
        .unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_kala"))
        .arg("repl")
        .arg(&fx.path)
        .stdin(Stdio::piped())
        .stdout(output)
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(format!("{QUERY}\n").as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    assert_eq!(code(&out), 3, "{}", stderr(&out));
}

#[cfg(unix)]
#[test]
fn oversized_history_is_rejected_without_truncation() {
    let fx = varrow();
    let path = fx.dir.path().join("large-history");
    let file = std::fs::File::create(&path).unwrap();
    file.set_len(1024 * 1024 + 1).unwrap();
    drop(file);
    let o = repl(&fx, b".quit\n", &["--history", path.to_str().unwrap()]);
    assert_ne!(code(&o), 0);
    assert_eq!(std::fs::metadata(&path).unwrap().len(), 1024 * 1024 + 1);
}

// Review round 1: history is saved even when the session ends on an output
// error (stdout on a full device).
#[cfg(target_os = "linux")]
#[test]
fn history_is_saved_when_stdout_fails() {
    use std::io::Write;
    use std::process::{Command, Stdio};
    let fx = varrow();
    let history = fx.dir.path().join("history");
    let mut child = Command::new(env!("CARGO_BIN_EXE_kala"))
        .arg("repl")
        .arg(&fx.path)
        .args(["--history", history.to_str().unwrap()])
        .stdin(Stdio::piped())
        .stdout(std::fs::File::create("/dev/full").unwrap())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"[:find ?a :at -210 :where [:edwin :ally_of ?a]]\n")
        .unwrap();
    let o = child.wait_with_output().unwrap();
    assert_eq!(o.status.code(), Some(3));
    let saved = std::fs::read_to_string(&history).unwrap();
    assert!(saved.contains(":edwin :ally_of"), "history: {saved:?}");
}

// Review round 2: a save cut short (e.g. a full disk) leaves an unterminated
// last line; the next start drops that line instead of refusing to start.
#[test]
fn history_with_a_cut_off_last_line_still_opens() {
    let fx = varrow();
    let history = fx.dir.path().join("history");
    std::fs::write(
        &history,
        "\"[:find ?a :where [:edwin :ally_of ?a]]\"\n\"[:find ?b",
    )
    .unwrap();
    let o = repl(&fx, b".quit\n", &["--history", history.to_str().unwrap()]);
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    let saved = std::fs::read_to_string(&history).unwrap();
    assert_eq!(
        saved,
        "\"[:find ?a :where [:edwin :ally_of ?a]]\"\n\".quit\"\n"
    );
}

// A complete (newline-terminated) bad line is still refused.
#[test]
fn history_with_a_bad_complete_line_is_refused() {
    let fx = varrow();
    let history = fx.dir.path().join("history");
    std::fs::write(&history, "\"ok\"\nnot json\n").unwrap();
    let o = repl(&fx, b".quit\n", &["--history", history.to_str().unwrap()]);
    assert_ne!(code(&o), 0);
}

/// The helpers that pipe input to `kala` must not fail a test because the
/// child exited before reading it (a refused world, a usage error): the
/// write then hits a closed pipe. This was a 1-in-150 flake of
/// `cli_schema.rs::schema_forms_cannot_write_the_system_world_even_after_rename`;
/// here the child is known to be gone before the write.
#[test]
fn feeding_a_child_that_already_exited_is_not_a_test_failure() {
    let mut child = Command::new(env!("CARGO_BIN_EXE_kala"))
        .args(["repl", "/nonexistent/world.kala", "--write"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let started = std::time::Instant::now();
    while child.try_wait().unwrap().is_none() {
        assert!(started.elapsed() < std::time::Duration::from_secs(20));
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    feed(&mut child, b"[:assert :greg :age 1]\n.quit\n");
    let o = child.wait_with_output().unwrap();
    assert_eq!(code(&o), 1, "{}", stderr(&o));
}
