//! Slice 5 item 2: `kala load <fixture.yaml> <world.kala> [--force]`.

mod common;
use common::*;
use kala_core::{World, load_fixture};
use std::fs;

fn varrow_mem() -> World {
    load_fixture(&fs::read_to_string(VARROW).unwrap()).unwrap()
}

fn load<'a>(
    fixture: &'a std::path::Path,
    world: &'a std::path::Path,
    extra: &[&str],
) -> std::process::Output {
    let mut args: Vec<&std::ffi::OsStr> =
        vec!["load".as_ref(), fixture.as_os_str(), world.as_os_str()];
    args.extend(extra.iter().map(|s| std::ffi::OsStr::new(*s)));
    kala(args)
}

fn write_fixture(dir: &std::path::Path, name: &str, yaml: &str) -> std::path::PathBuf {
    let p = dir.join(name);
    fs::write(&p, yaml).unwrap();
    p
}

#[test]
fn load_varrow_builds_a_world_and_prints_counts() {
    let dir = tempfile::tempdir().unwrap();
    let world = dir.path().join("varrow.kala");
    let o = load(VARROW.as_ref(), &world, &[]);
    assert_eq!(code(&o), 0, "stderr: {}", stderr(&o));
    let out = stdout(&o);

    let mem = varrow_mem();
    let c = counts(&mem);
    assert!(c.entities > 0 && c.facts > 0 && c.transactions > 1);
    assert!(
        out.contains(&format!("{} entities", c.entities)),
        "stdout: {out}"
    );
    assert!(out.contains(&format!("{} facts", c.facts)), "stdout: {out}");
    assert!(
        out.contains(&format!("{} transactions", c.transactions)),
        "stdout: {out}"
    );

    let opened = World::open(&world).expect("loaded world opens");
    assert_eq!(opened.latest_tx(), mem.latest_tx());
    assert_eq!(opened.latest_tick(), mem.latest_tick());
    assert_eq!(counts(&opened).facts, c.facts);
}

#[test]
fn load_varrow_prints_one_warning_line_per_overlap() {
    let dir = tempfile::tempdir().unwrap();
    let world = dir.path().join("varrow.kala");
    let o = load(VARROW.as_ref(), &world, &[]);
    assert_eq!(code(&o), 0, "stderr: {}", stderr(&o));
    let out = stdout(&o);
    assert!(out.contains(" entities"), "summary missing: {out}");
    let expected = overlap_pairs(&varrow_mem());
    let lines = warning_lines(&out);
    assert_eq!(lines.len(), expected.len(), "stdout: {out}");
    for (handle, attr) in &expected {
        assert!(
            lines
                .iter()
                .any(|l| l.contains(handle.as_str()) && l.contains(attr.as_str())),
            "no warning naming {handle} / {attr}: {out}"
        );
    }
}

#[test]
fn load_warns_about_an_unresolved_overlap() {
    let dir = tempfile::tempdir().unwrap();
    let fx = write_fixture(dir.path(), "f.yaml", OVERLAP_FIXTURE);
    let world = dir.path().join("w.kala");
    let o = load(&fx, &world, &[]);
    assert_eq!(code(&o), 0, "stderr: {}", stderr(&o));
    let out = stdout(&o);
    let lines = warning_lines(&out);
    assert_eq!(lines.len(), 1, "stdout: {out}");
    assert!(
        lines[0].contains("greg") && lines[0].contains("located_in"),
        "{}",
        lines[0]
    );
}

#[test]
fn load_prints_no_warning_when_the_overlap_is_resolved() {
    let dir = tempfile::tempdir().unwrap();
    let fx = write_fixture(dir.path(), "f.yaml", RESOLVED_FIXTURE);
    let world = dir.path().join("w.kala");
    let o = load(&fx, &world, &[]);
    assert_eq!(code(&o), 0, "stderr: {}", stderr(&o));
    let out = stdout(&o);
    assert!(
        out.contains(" entities") && out.contains(" facts") && out.contains(" transactions"),
        "{out}"
    );
    assert!(warning_lines(&out).is_empty(), "stdout: {out}");
}

#[test]
fn load_creates_only_the_world_file() {
    let dir = tempfile::tempdir().unwrap();
    let fx = write_fixture(
        &{
            let d = dir.path().join("in");
            fs::create_dir(&d).unwrap();
            d
        },
        "f.yaml",
        OTHER_FIXTURE,
    );
    let out_dir = dir.path().join("out");
    fs::create_dir(&out_dir).unwrap();
    let world = out_dir.join("w.kala");
    let o = load(&fx, &world, &[]);
    assert_eq!(code(&o), 0, "stderr: {}", stderr(&o));
    // World::create writes a single file (the lock is advisory, on that file).
    assert_eq!(dir_names(&out_dir), vec!["w.kala".to_string()]);
}

#[test]
fn load_refuses_to_overwrite_an_existing_world() {
    let dir = tempfile::tempdir().unwrap();
    let fx = write_fixture(dir.path(), "f.yaml", OTHER_FIXTURE);
    let world = dir.path().join("w.kala");
    assert_eq!(code(&load(&fx, &world, &[])), 0);
    let before = fs::read(&world).unwrap();
    let before_names = dir_names(dir.path());

    let o = load(VARROW.as_ref(), &world, &[]);
    assert_eq!(code(&o), 1, "stderr: {}", stderr(&o));
    assert!(o.stdout.is_empty(), "stdout: {}", stdout(&o));
    let err = stderr(&o);
    assert!(
        err.starts_with("kala: error: ") && err.contains("--force"),
        "stderr: {err}"
    );
    assert_eq!(fs::read(&world).unwrap(), before);
    assert_eq!(dir_names(dir.path()), before_names);
}

#[test]
fn load_force_replaces_an_existing_world() {
    let dir = tempfile::tempdir().unwrap();
    let world = dir.path().join("w.kala");
    assert_eq!(code(&load(VARROW.as_ref(), &world, &[])), 0);
    let fx = write_fixture(dir.path(), "f.yaml", OTHER_FIXTURE);
    let names_before = dir_names(dir.path());

    let o = load(&fx, &world, &["--force"]);
    assert_eq!(code(&o), 0, "stderr: {}", stderr(&o));
    let out = stdout(&o);
    assert!(out.contains("2 entities"), "stdout: {out}");

    let w = World::open(&world).unwrap();
    assert!(w.entity_by_handle("zed").is_some());
    assert!(w.entity_by_handle("edwin").is_none());
    assert_eq!(
        w.latest_tick(),
        load_fixture(OTHER_FIXTURE).unwrap().latest_tick()
    );
    drop(w);
    assert_eq!(
        dir_names(dir.path()),
        names_before,
        "no leftover temp files"
    );
}

#[test]
fn load_force_with_a_failing_fixture_keeps_the_old_world() {
    let dir = tempfile::tempdir().unwrap();
    let inp = dir.path().join("in");
    let outd = dir.path().join("out");
    fs::create_dir(&inp).unwrap();
    fs::create_dir(&outd).unwrap();
    let world = outd.join("w.kala");
    let good = write_fixture(&inp, "good.yaml", OTHER_FIXTURE);
    assert_eq!(code(&load(&good, &world, &[])), 0);
    let before = fs::read(&world).unwrap();

    let bad = write_fixture(&inp, "bad.yaml", BAD_OP_FIXTURE);
    let o = load(&bad, &world, &["--force"]);
    assert_eq!(code(&o), 1, "stderr: {}", stderr(&o));
    assert!(o.stdout.is_empty());
    assert!(stderr(&o).contains("nobody"), "stderr: {}", stderr(&o));
    assert_eq!(
        fs::read(&world).unwrap(),
        before,
        "old world must be intact"
    );
    assert_eq!(
        dir_names(&outd),
        vec!["w.kala".to_string()],
        "no temp files left"
    );
}

#[test]
fn fixture_error_reports_position_label_and_leaves_no_world() {
    let dir = tempfile::tempdir().unwrap();
    let inp = dir.path().join("in");
    let outd = dir.path().join("out");
    fs::create_dir(&inp).unwrap();
    fs::create_dir(&outd).unwrap();
    let fx = write_fixture(&inp, "bad.yaml", BAD_OP_FIXTURE);
    let world = outd.join("w.kala");
    let o = load(&fx, &world, &[]);
    assert_eq!(code(&o), 1, "stderr: {}", stderr(&o));
    assert!(o.stdout.is_empty(), "stdout: {}", stdout(&o));
    let err = stderr(&o);
    assert!(err.starts_with("kala: error: "), "stderr: {err}");
    assert!(err.contains("nobody"), "stderr: {err}");
    assert!(err.contains("bad-op"), "stderr: {err}");
    assert!(
        err.contains("transaction 2, op 1"),
        "positions are 1-based: {err}"
    );
    assert!(
        dir_names(&outd).is_empty(),
        "no world or temp file left: {:?}",
        dir_names(&outd)
    );
}

#[test]
fn malformed_yaml_is_a_user_error() {
    let dir = tempfile::tempdir().unwrap();
    let fx = write_fixture(dir.path(), "f.yaml", "schema: [unclosed\n");
    let world = dir.path().join("w.kala");
    let o = load(&fx, &world, &[]);
    assert_eq!(code(&o), 1, "stderr: {}", stderr(&o));
    assert!(stderr(&o).starts_with("kala: error: "));
    assert!(!world.exists());
}

#[test]
fn nonexistent_fixture_names_the_path() {
    let dir = tempfile::tempdir().unwrap();
    let fx = dir.path().join("missing.yaml");
    let world = dir.path().join("w.kala");
    let o = load(&fx, &world, &[]);
    assert_eq!(code(&o), 1, "stderr: {}", stderr(&o));
    assert!(o.stdout.is_empty());
    let err = stderr(&o);
    assert!(
        err.starts_with("kala: error: ") && err.contains("missing.yaml"),
        "stderr: {err}"
    );
    assert!(!world.exists());
}

#[test]
fn missing_world_argument_is_a_usage_error() {
    let o = kala(["load", VARROW]);
    assert_eq!(code(&o), 2, "stderr: {}", stderr(&o));
    assert!(o.stdout.is_empty());
    assert!(!stderr(&o).is_empty());
}
