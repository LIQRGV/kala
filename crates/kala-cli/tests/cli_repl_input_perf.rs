//! Exercise the real REPL input loop: many short lines must be linear.
mod common;

use common::{code, stderr, stdout};
use kala_core::{Schema, World};
use std::io::Write;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

fn input_run(lines: usize) -> Duration {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("empty.kala");
    drop(World::create(&path, Schema::new().node_type("THING")).unwrap());
    // The opening string keeps every following one-character line pending.
    // Cancel avoids making parsing/execution part of this input benchmark.
    let input = format!("[\"\n{}.cancel\n.quit\n", "x\n".repeat(lines));
    let start = Instant::now();
    let mut child = Command::new(env!("CARGO_BIN_EXE_kala"))
        .arg("repl")
        .arg(&path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(input.as_bytes())
        .unwrap();
    let result = child.wait_with_output().unwrap();
    let elapsed = start.elapsed();
    assert_eq!(code(&result), 0, "{}", stderr(&result));
    assert_eq!(stdout(&result), "");
    assert_eq!(stderr(&result), "");
    elapsed
}

#[test]
fn short_line_input_correctness_twin() {
    input_run(32);
}

#[test]
#[cfg_attr(debug_assertions, ignore)]
fn sixty_four_kib_of_one_character_lines_finish_under_two_hundred_ms() {
    let elapsed = input_run((kala_datalog::MAX_QUERY_BYTES - 3) / 2);
    assert!(
        elapsed < Duration::from_millis(200),
        "REPL short-line input took {elapsed:?}"
    );
}
