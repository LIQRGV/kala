//! PR #7 review round 1: restore diagnostics, file modes, genesis parsing,
//! reserved names, export recovery warnings and readable dump subjects.
mod common;

use common::{VARROW, code, kala, stderr, stdout};
use std::fs;
use std::path::Path;

fn s(p: &Path) -> &str {
    p.to_str().unwrap()
}

fn ok(o: &std::process::Output) {
    assert_eq!(code(o), 0, "stdout: {}\nstderr: {}", stdout(o), stderr(o));
}

#[test]
fn ordinary_restore_failure_names_its_reason() {
    let dir = tempfile::tempdir().unwrap();
    let dump = dir.path().join("bad.edn");
    fs::write(&dump, "[:genesis]\n[:assert :nobody :x 1 :at 0]\n").unwrap();
    let target = dir.path().join("bad.kala");
    let o = kala(["transact", "--create", s(&target), s(&dump)]);
    assert_ne!(code(&o), 0);
    let err = stderr(&o);
    assert!(err.contains("nobody"), "the author needs the reason: {err}");
    assert!(!err.contains("content withheld"), "{err}");
    assert!(!target.exists());
}

#[cfg(unix)]
#[test]
fn ordinary_worlds_keep_the_default_file_mode() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let probe = dir.path().join("probe");
    fs::File::create(&probe).unwrap();
    let default = fs::metadata(&probe).unwrap().permissions().mode() & 0o777;
    let loaded = dir.path().join("v.kala");
    ok(&kala(["load", VARROW, s(&loaded)]));
    let created = dir.path().join("n.kala");
    ok(&kala(["create", s(&created)]));
    let dump = dir.path().join("v.edn");
    ok(&kala(["dump", s(&loaded), "-o", s(&dump)]));
    let restored = dir.path().join("r.kala");
    ok(&kala(["transact", "--create", s(&restored), s(&dump)]));
    for path in [&loaded, &created, &restored] {
        let mode = fs::metadata(path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, default, "{} has mode {mode:o}", path.display());
    }
    // Dump files are always private: they are backups.
    let mode = fs::metadata(&dump).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600);
}

#[test]
fn a_repeated_genesis_setting_is_a_no_op_like_a_live_one() {
    let dir = tempfile::tempdir().unwrap();
    let schema = dir.path().join("schema.edn");
    fs::write(
        &schema,
        "[:world-setting :schema :strict]\n[:world-setting :schema :strict]\n[:node-type :CHARACTER]\n",
    )
    .unwrap();
    let world = dir.path().join("w.kala");
    ok(&kala(["create", s(&world), "--schema", s(&schema)]));
    let out = stdout(&kala(["schema", s(&world)]));
    assert!(out.contains(":schema :strict"), "{out}");
}

#[test]
fn genesis_with_a_bare_operation_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let schema = dir.path().join("schema.edn");
    fs::write(
        &schema,
        "[:genesis :world-setting :schema :strict]\n[:node-type :CHARACTER]\n",
    )
    .unwrap();
    let world = dir.path().join("w.kala");
    let o = kala(["create", s(&world), "--schema", s(&schema)]);
    assert_ne!(code(&o), 0, "{}", stdout(&o));
    assert!(!world.exists());
    // The vector form is the genesis.
    fs::write(
        &schema,
        "[:genesis [:world-setting :schema :strict] [:node-type :CHARACTER]]\n",
    )
    .unwrap();
    ok(&kala(["create", s(&world), "--schema", s(&schema)]));
}

#[test]
fn an_inverse_may_be_named_inferred_and_the_marker_is_position_free() {
    let dir = tempfile::tempdir().unwrap();
    let world = dir.path().join("w.kala");
    ok(&kala(["create", s(&world)]));
    let forms = dir.path().join("f.edn");
    fs::write(
        &forms,
        "[:attribute :parent_of :ref :directed :inverse :inferred]\n[:attribute :age :i64 :inferred :one]\n",
    )
    .unwrap();
    ok(&kala(["transact", s(&world), s(&forms)]));
    let out = stdout(&kala(["schema", s(&world)]));
    assert!(out.contains("inverse :inferred"), "{out}");
    let age = out.lines().find(|l| l.contains(":age")).unwrap();
    assert!(age.contains("inferred"), "{age}");
}

#[test]
fn create_schema_refuses_the_reserved_prefix() {
    let dir = tempfile::tempdir().unwrap();
    for form in [
        "[:attribute :_KALA_grant :string]",
        "[:node-type :_KALA_THING]",
    ] {
        let schema = dir.path().join("schema.edn");
        fs::write(&schema, form).unwrap();
        let world = dir.path().join("w.kala");
        let o = kala(["create", s(&world), "--schema", s(&schema)]);
        assert_ne!(code(&o), 0, "{form} must be refused");
        assert!(!world.exists());
    }
}

#[test]
fn export_warns_about_an_omitted_tail() {
    let dir = tempfile::tempdir().unwrap();
    let world = dir.path().join("v.kala");
    ok(&kala(["load", VARROW, s(&world)]));
    let mut bytes = fs::read(&world).unwrap();
    bytes.extend_from_slice(&[7, 1, 2, 3]);
    fs::write(&world, bytes).unwrap();
    let o = kala(["export", s(&world)]);
    ok(&o);
    assert!(stderr(&o).contains("omitted 4 bytes"), "{}", stderr(&o));
}

#[test]
fn dumps_name_entities_by_handle() {
    let dir = tempfile::tempdir().unwrap();
    let world = dir.path().join("v.kala");
    ok(&kala(["load", VARROW, s(&world)]));
    let o = kala(["dump", s(&world)]);
    ok(&o);
    let text = stdout(&o);
    assert!(
        text.contains("[:assert :edwin :ally_of :mara "),
        "subjects and refs use handles"
    );
    assert!(
        !text.contains("[:assert CHARACTER#"),
        "every varrow character has a handle"
    );
}

#[test]
fn handles_that_look_like_syntax_roundtrip_by_handle() {
    let dir = tempfile::tempdir().unwrap();
    let world = dir.path().join("w.kala");
    ok(&kala(["create", s(&world)]));
    let forms = dir.path().join("f.edn");
    fs::write(
        &forms,
        "[[:create :T :at] [:create :T :id] [:create :T :-1] [:create :T :a/b] [:create :T :é] [:create :T :genesis]]\n\
         [[:assert :at :knows :id :at 1] [:assert :-1 :knows :at :at 2] [:assert :a/b :knows :é :at 3] [:assert :genesis :knows :-1 :at 4] [:assert :at :n 1 :at 5]]\n",
    )
    .unwrap();
    ok(&kala(["transact", s(&world), s(&forms)]));
    let first = stdout(&kala(["log", s(&world)]));
    let corrections = dir.path().join("c.edn");
    let entry = first
        .lines()
        .find(|l| l.contains("assert at knows id"))
        .and_then(|l| l.split_whitespace().next())
        .unwrap()
        .trim_start_matches("entry#")
        .to_owned();
    fs::write(
        &corrections,
        format!("[:correct {entry} :replace :value :at :at 1]\n"),
    )
    .unwrap();
    ok(&kala(["transact", s(&world), s(&corrections)]));
    let dump = dir.path().join("w.edn");
    ok(&kala(["dump", s(&world), "-o", s(&dump)]));
    let text = fs::read_to_string(&dump).unwrap();
    assert!(text.contains("[:assert :-1 :knows :at "), "{text}");
    let replay = dir.path().join("r.kala");
    ok(&kala(["transact", "--create", s(&replay), s(&dump)]));
    assert!(fs::read(&world).unwrap() == fs::read(&replay).unwrap());
}
