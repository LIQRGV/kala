//! Slice 5 item 1: the `kala` binary's general behaviour (help, version,
//! exit codes, error format, output hygiene).

mod common;
use common::*;
use std::ffi::OsString;
use std::os::unix::ffi::OsStringExt;

const SUBCOMMANDS: [&str; 10] = [
    "load",
    "asof",
    "neighbors",
    "history",
    "find",
    "list",
    "edge",
    "doctor",
    "log",
    "info",
];

fn check_usage(o: &std::process::Output) {
    let out = stdout(o);
    for s in SUBCOMMANDS {
        assert!(
            out.lines().any(|l| l.split_whitespace().next() == Some(s)),
            "help should list subcommand `{s}` on its own line:\n{out}"
        );
    }
    assert!(!o.stdout.contains(&0x1b) && !o.stderr.contains(&0x1b));
}

#[test]
fn help_flag_lists_every_subcommand() {
    let o = kala(["--help"]);
    assert_eq!(code(&o), 0, "stderr: {}", stderr(&o));
    check_usage(&o);
}

#[test]
fn help_subcommand_lists_every_subcommand() {
    let o = kala(["help"]);
    assert_eq!(code(&o), 0, "stderr: {}", stderr(&o));
    check_usage(&o);
}

#[test]
fn version_prints_name_and_workspace_version() {
    let o = kala(["--version"]);
    assert_eq!(code(&o), 0);
    assert!(stdout(&o).contains("kala 0.1.0"), "stdout: {}", stdout(&o));
    assert_eq!(env!("CARGO_PKG_VERSION"), "0.1.0");
}

fn assert_usage_error(o: &std::process::Output) {
    assert_eq!(code(o), 2, "stderr: {}", stderr(o));
    assert!(
        o.stdout.is_empty(),
        "stdout must be empty on error: {}",
        stdout(o)
    );
    assert!(
        !stderr(o).trim().is_empty(),
        "usage error must explain itself"
    );
    assert!(!o.stderr.contains(&0x1b), "no ANSI styling in usage errors");
}

#[test]
fn no_subcommand_is_a_usage_error() {
    assert_usage_error(&kala::<[&str; 0], &str>([]));
}

#[test]
fn unknown_subcommand_is_a_usage_error() {
    let o = kala(["frobnicate"]);
    assert_usage_error(&o);
    assert!(stderr(&o).contains("frobnicate"));
}

#[test]
fn unknown_flag_is_a_usage_error() {
    let o = kala(["load", "--bogus-flag", "a.yaml", "w.kala"]);
    assert_usage_error(&o);
    assert!(stderr(&o).contains("bogus-flag"));
}

#[test]
fn missing_argument_is_a_usage_error() {
    assert_usage_error(&kala(["load", "only-one-arg.yaml"]));
}

#[test]
fn malformed_typed_argument_is_a_usage_error() {
    // `--limit` is a number (item 6); a non-number is a usage error at parse time.
    let o = kala(["log", "some.kala", "--limit", "many"]);
    assert_usage_error(&o);
    assert!(stderr(&o).contains("many"));
}

#[test]
fn esc_in_unknown_subcommand_is_escaped() {
    let arg = OsString::from_vec(b"\x1b[31mevil".to_vec());
    let o = kala([arg]);
    assert_usage_error(&o);
    let err = stderr(&o);
    assert!(err.contains("\\u{1b}"), "expected escaped ESC in: {err:?}");
    no_raw_controls(&err);
}

#[test]
fn c1_csi_in_unknown_argument_is_escaped() {
    let arg = OsString::from("--\u{9b}31mevil");
    let o = kala([OsString::from("load"), arg]);
    assert_usage_error(&o);
    let err = stderr(&o);
    assert!(
        err.contains("\\u{9b}"),
        "expected escaped U+009B in: {err:?}"
    );
    assert!(
        !o.stderr.windows(2).any(|w| w == [0xc2, 0x9b]),
        "raw C1 CSI bytes in {err:?}"
    );
    no_raw_controls(&err);
}

#[test]
fn esc_in_nonexistent_fixture_path_is_escaped_in_load_error() {
    let dir = tempfile::tempdir().unwrap();
    let fx = dir.path().join("no\u{1b}[31mpe.yaml");
    let world = dir.path().join("w.kala");
    let o = kala(
        [fx.as_os_str(), world.as_os_str()]
            .map(|s| s.to_owned())
            .into_iter()
            .fold(vec![OsString::from("load")], |mut v, s| {
                v.push(s);
                v
            }),
    );
    assert_eq!(code(&o), 1, "stderr: {}", stderr(&o));
    assert!(o.stdout.is_empty());
    let err = stderr(&o);
    assert!(err.starts_with("kala: error: "), "stderr: {err:?}");
    assert!(err.contains("\\u{1b}"), "expected escaped ESC in: {err:?}");
    no_raw_controls(&err);
    assert!(!world.exists());
}

#[test]
fn esc_in_world_path_in_missing_directory_is_escaped() {
    let dir = tempfile::tempdir().unwrap();
    let world = dir.path().join("no\u{1b}[31mdir").join("w.kala");
    let o = kala([
        OsString::from("load"),
        OsString::from(common::VARROW),
        world.into_os_string(),
    ]);
    assert_eq!(code(&o), 1, "stderr: {}", stderr(&o));
    assert!(o.stdout.is_empty());
    let err = stderr(&o);
    assert!(err.starts_with("kala: error: "), "stderr: {err:?}");
    no_raw_controls(&err);
}
