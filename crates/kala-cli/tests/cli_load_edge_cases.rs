//! Slice 5 item 2, edge cases: `kala load` paths, size cap, encodings,
//! escaping of echoed input, store I/O failures and the world lock.

mod common;
use common::*;
use kala_core::World;
use kala_fixture::MAX_FIXTURE_BYTES;
use kala_fixture::load_fixture_to;
use std::ffi::OsStr;
use std::fs;
use std::path::Path;

fn load(fixture: &Path, world: &OsStr, extra: &[&str]) -> std::process::Output {
    let mut args: Vec<&OsStr> = vec!["load".as_ref(), fixture.as_os_str(), world];
    args.extend(extra.iter().map(|s| OsStr::new(*s)));
    kala(args)
}

fn fixture_file(dir: &Path, yaml: &str) -> std::path::PathBuf {
    let p = dir.join("f.yaml");
    fs::write(&p, yaml).unwrap();
    p
}

fn assert_exit1(o: &std::process::Output) {
    assert_eq!(code(o), 1, "stderr: {}", stderr(o));
    assert!(o.stdout.is_empty(), "stdout: {}", stdout(o));
    assert!(stderr(o).starts_with("kala: error: "), "{}", stderr(o));
    no_raw_controls(&stderr(o));
}

#[test]
fn world_path_without_file_name_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let f = fixture_file(dir.path(), OTHER_FIXTURE);
    let o = load(&f, OsStr::new("/"), &[]);
    assert_exit1(&o);
    let o = load(&f, OsStr::new("/"), &["--force"]);
    assert_exit1(&o);
    assert_eq!(dir_names(dir.path()), vec!["f.yaml"]);
}

#[test]
fn world_path_ending_in_a_slash_is_refused_and_creates_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let f = fixture_file(dir.path(), OTHER_FIXTURE);
    let world = format!("{}/newworld/", dir.path().display());
    let o = load(&f, OsStr::new(&world), &[]);
    assert_exit1(&o);
    assert_eq!(dir_names(dir.path()), vec!["f.yaml"]);
}

#[test]
fn world_path_that_is_a_directory_is_never_replaced() {
    for extra in [&[][..], &["--force"][..]] {
        let dir = tempfile::tempdir().unwrap();
        let f = fixture_file(dir.path(), OTHER_FIXTURE);
        let target = dir.path().join("w.kala");
        fs::create_dir(&target).unwrap();
        fs::write(target.join("keep.txt"), "precious").unwrap();
        let o = load(&f, target.as_os_str(), extra);
        assert_exit1(&o);
        assert!(target.is_dir(), "directory was replaced ({extra:?})");
        assert_eq!(dir_names(&target), vec!["keep.txt"]);
        assert_eq!(
            fs::read_to_string(target.join("keep.txt")).unwrap(),
            "precious"
        );
        assert_eq!(dir_names(dir.path()), vec!["f.yaml", "w.kala"], "{extra:?}");
    }
}

#[cfg(unix)]
#[test]
fn dangling_symlink_target_is_refused_without_force() {
    let dir = tempfile::tempdir().unwrap();
    let f = fixture_file(dir.path(), OTHER_FIXTURE);
    let target = dir.path().join("w.kala");
    let dest = dir.path().join("nowhere");
    std::os::unix::fs::symlink(&dest, &target).unwrap();
    let o = load(&f, target.as_os_str(), &[]);
    assert_exit1(&o);
    assert!(!dest.exists(), "the symlink target was created");
    assert!(
        fs::symlink_metadata(&target)
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert_eq!(dir_names(dir.path()), vec!["f.yaml", "w.kala"]);
}

fn padded(total: usize) -> String {
    let mut s = String::from(OTHER_FIXTURE);
    s.push_str("\n#");
    let pad = total - s.len();
    s.push_str(&"x".repeat(pad));
    assert_eq!(s.len(), total);
    s
}

#[test]
fn fixture_exactly_at_the_size_cap_is_not_refused_for_size() {
    let dir = tempfile::tempdir().unwrap();
    let f = fixture_file(dir.path(), &padded(MAX_FIXTURE_BYTES));
    let world = dir.path().join("w.kala");
    let o = load(&f, world.as_os_str(), &[]);
    let err = stderr(&o);
    assert!(
        !err.contains("limit") && !err.contains("larger"),
        "refused for size at exactly the cap: {err}"
    );
    assert_eq!(code(&o), 0, "stderr: {err}");
    assert!(world.exists());
}

#[test]
fn fixture_one_byte_over_the_cap_is_refused_for_size() {
    let dir = tempfile::tempdir().unwrap();
    let f = fixture_file(dir.path(), &padded(MAX_FIXTURE_BYTES + 1));
    let world = dir.path().join("w.kala");
    let o = load(&f, world.as_os_str(), &[]);
    assert_exit1(&o);
    let err = stderr(&o);
    assert!(
        err.contains(&MAX_FIXTURE_BYTES.to_string()) || err.contains("larger"),
        "no mention of size: {err}"
    );
    assert_eq!(dir_names(dir.path()), vec!["f.yaml"]);
}

#[test]
fn non_utf8_fixture_is_a_user_error() {
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("f.yaml");
    let mut bytes = OTHER_FIXTURE.as_bytes().to_vec();
    bytes.extend_from_slice(b"\n# \xff\xfe\n");
    fs::write(&f, bytes).unwrap();
    let world = dir.path().join("w.kala");
    let o = load(&f, world.as_os_str(), &[]);
    assert_exit1(&o);
    assert_eq!(dir_names(dir.path()), vec!["f.yaml"]);
}

#[cfg(unix)]
#[test]
fn non_utf8_argument_in_a_usage_error_is_exit_2_without_panic() {
    use std::os::unix::ffi::OsStrExt;
    let bad = OsStr::from_bytes(b"bad\xff\xfename");
    let o = kala([bad]);
    assert_eq!(code(&o), 2, "stderr: {}", stderr(&o));
    assert!(o.stdout.is_empty());
    let err = stderr(&o);
    assert!(!err.contains("panicked"), "{err}");
    no_raw_controls(&err);
    // And as a load argument.
    let o = kala([OsStr::new("load"), bad, bad, OsStr::new("--bogus")]);
    assert_eq!(code(&o), 2, "stderr: {}", stderr(&o));
    assert!(!stderr(&o).contains("panicked"));
}

#[test]
fn newline_in_an_unknown_subcommand_is_escaped() {
    let o = kala(["bad\nname"]);
    assert_eq!(code(&o), 2, "stderr: {}", stderr(&o));
    let err = stderr(&o);
    assert!(err.contains("bad\\nname"), "stderr: {err:?}");
    assert!(!err.contains("bad\nname"), "raw newline echoed: {err:?}");
}

#[test]
fn very_long_unknown_subcommand_is_cut_with_an_ellipsis() {
    let long = "a".repeat(300);
    let o = kala([long.as_str()]);
    assert_eq!(code(&o), 2, "stderr: {}", stderr(&o));
    let err = stderr(&o);
    assert!(err.contains("..."), "stderr: {err}");
    assert!(!err.contains(&"a".repeat(250)), "echoed uncut: {err}");
}

#[test]
fn reverse_side_overlap_prints_a_warning_line() {
    const Y: &str = r#"
schema:
  node_types: [CHARACTER, PLACE]
  attributes:
    - { name: ruled_by, type: ref, reverse: one }
entities:
  CHARACTER: [lisa, edwin]
  PLACE: [ashford]
transactions:
  - tick: 10
    ops:
      - assert: [ashford, ruled_by, lisa]
      - assert: [ashford, ruled_by, edwin]
"#;
    let dir = tempfile::tempdir().unwrap();
    let f = fixture_file(dir.path(), Y);
    let world = dir.path().join("w.kala");
    let o = load(&f, world.as_os_str(), &[]);
    assert_eq!(code(&o), 0, "stderr: {}", stderr(&o));
    let out = stdout(&o);
    let warns = warning_lines(&out);
    assert!(!warns.is_empty(), "no warning line: {out}");
    assert!(warns.iter().any(|l| l.contains("ruled_by")), "{warns:?}");
}

#[cfg(unix)]
#[test]
fn read_only_output_directory_is_a_store_error_and_leaves_nothing() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let f = fixture_file(dir.path(), OTHER_FIXTURE);
    let out_dir = dir.path().join("ro");
    fs::create_dir(&out_dir).unwrap();
    fs::set_permissions(&out_dir, fs::Permissions::from_mode(0o555)).unwrap();
    // Running as root ignores the mode bits: skip then.
    if fs::write(out_dir.join("probe"), "x").is_ok() {
        fs::set_permissions(&out_dir, fs::Permissions::from_mode(0o755)).unwrap();
        eprintln!("skipped: permissions are not enforced (root?)");
        return;
    }
    let world = out_dir.join("w.kala");
    let o = load(&f, world.as_os_str(), &[]);
    fs::set_permissions(&out_dir, fs::Permissions::from_mode(0o755)).unwrap();
    // Spec: a store I/O failure is exit 3.
    assert_eq!(code(&o), 3, "stderr: {}", stderr(&o));
    assert!(o.stdout.is_empty());
    no_raw_controls(&stderr(&o));
    assert!(
        dir_names(&out_dir).is_empty(),
        "left behind: {:?}",
        dir_names(&out_dir)
    );
}

/// Slice 15b: the lock is held per transaction, so what `--force` must not
/// replace under is a commit in progress (here: the lock itself, held by
/// this process), after waiting the busy timeout.
#[test]
fn force_while_the_world_is_open_in_another_process_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let f = fixture_file(dir.path(), RESOLVED_FIXTURE);
    let path = dir.path().join("w.kala");
    let held: World = load_fixture_to(OTHER_FIXTURE, &path).unwrap();
    let before = fs::read(&path).unwrap();
    let lock = fs::File::open(&path).unwrap();
    lock.try_lock().expect("the lock is free");
    let o = std::process::Command::new(env!("CARGO_BIN_EXE_kala"))
        .arg("load")
        .arg(&f)
        .arg(&path)
        .arg("--force")
        .args(["--busy-timeout", "100ms"])
        .output()
        .unwrap();
    assert_exit1(&o);
    assert_eq!(fs::read(&path).unwrap(), before, "old world was replaced");
    assert_eq!(held.latest_tick().0, 5);
    drop(lock);
    drop(held);
    // Only the world and the fixture remain (no temp file).
    assert_eq!(dir_names(dir.path()), vec!["f.yaml", "w.kala"]);
}

/// A process that merely has the old world open holds no lock any more: the
/// world is replaced, and that handle refuses to write to a log that is no
/// longer its own instead of appending to the replaced (unlinked) file.
#[test]
fn force_replaces_under_an_idle_handle_which_then_refuses_to_write() {
    let dir = tempfile::tempdir().unwrap();
    let f = fixture_file(dir.path(), RESOLVED_FIXTURE);
    let path = dir.path().join("w.kala");
    let mut idle: World = load_fixture_to(OTHER_FIXTURE, &path).unwrap();
    let before = fs::read(&path).unwrap();
    let o = load(&f, path.as_os_str(), &["--force"]);
    assert_eq!(
        o.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&o.stderr)
    );
    let replaced = fs::read(&path).unwrap();
    assert_ne!(replaced, before, "the world was replaced");
    assert_eq!(idle.catch_up().unwrap_err(), kala_core::Error::LogDiverged);
    let mut txn = idle.begin();
    assert_eq!(
        txn.declare_node_type("FACTION").unwrap_err(),
        kala_core::Error::LogDiverged
    );
    drop(txn);
    assert_eq!(fs::read(&path).unwrap(), replaced, "never written");
    assert_eq!(dir_names(dir.path()), vec!["f.yaml", "w.kala"]);
}
