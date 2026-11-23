//! Naming and write-count contracts through the real CLI.
mod common;

use common::{VARROW, code, kala, stderr, stdout};
use serde_json::Value as Json;
use std::fs;
use std::path::Path;
use std::process::Output;

fn success(output: &Output) {
    assert_eq!(code(output), 0, "{}", stderr(output));
}

fn write(path: &Path, source: &str, json: bool) -> Output {
    let input = path.with_extension("edn");
    fs::write(&input, source).unwrap();
    let mut args = vec!["transact", path.to_str().unwrap(), input.to_str().unwrap()];
    if json {
        args.push("--json");
    }
    let output = kala(args);
    success(&output);
    output
}

#[test]
fn inferred_declaration_reports_declared_separately() {
    for json in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("world.kala");
        success(&kala(["create", path.to_str().unwrap()]));
        write(&path, "[:create :CHARACTER :ann]", false);
        let output = write(&path, "[:assert :ann :age 42 :at 0]", json);
        if json {
            let report: Json = serde_json::from_slice(&output.stdout).unwrap();
            assert_eq!(report["changed"], 1, "{report}");
            assert_eq!(report["declared"], 1, "{report}");
            assert_eq!(report["ops"][0]["changed"], 1, "{report}");
            assert_eq!(report["ops"][0]["declared"], 1, "{report}");
        } else {
            let text = stdout(&output);
            assert!(text.contains("1 changed, 1 declared"), "{text}");
            assert_eq!(text.matches("1 changed, 1 declared").count(), 2, "{text}");
        }
    }
}

#[test]
fn kala_handle_replaces_handle() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("world.kala");
    success(&kala(["load", VARROW, path.to_str().unwrap()]));
    let query = "[:find ?e :where [?e :kala_handle \"edwin\"]]";
    let output = kala(["query", path.to_str().unwrap(), query, "--json"]);
    success(&output);
    let report: Json = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["rows"][0][0]["value"]["handle"], "edwin", "{report}");
    let output = kala(["explain", path.to_str().unwrap(), query]);
    success(&output);
    assert!(stdout(&output).contains(":kala_handle"));
    let output = kala([
        "query",
        path.to_str().unwrap(),
        "[:find ?e :where [?e :handle \"edwin\"]]",
    ]);
    assert_eq!(code(&output), 1, "{}", stdout(&output));
    let error = stderr(&output);
    assert!(error.contains("not defined"), "{error}");
    assert!(
        error.contains("the built-in handle attribute is now :kala_handle"),
        "{error}"
    );
}

#[test]
fn schema_fact_migration_and_noop_reports_keep_separate_counts() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("world.kala");
    success(&kala(["create", path.to_str().unwrap()]));
    let created: Json =
        serde_json::from_slice(&write(&path, "[:create :CHARACTER :ann]", true).stdout).unwrap();
    assert_eq!(
        (created["changed"].clone(), created["declared"].clone()),
        (0.into(), 1.into()),
        "{created}"
    );
    let declared = stdout(&write(&path, "[:attribute :title :string]", false));
    assert!(declared.contains("0 changed, 1 declared"), "{declared}");
    let fact = stdout(&write(&path, "[:assert :ann :title \"Ann\" :at -1]", false));
    assert!(fact.contains("1 changed"), "{fact}");
    assert!(!fact.contains("declared"), "{fact}");
    for (source, changed, declared) in [
        ("[:assert :ann :title \"Ann\" :at 0]", 0, 0),
        ("[:attribute :title :string]", 0, 0),
        ("[:assert :ann :age 42 :at 0]", 1, 1),
        ("[:migrate :age :string]", 1, 4),
        ("[:rename :title :name]", 0, 1),
        ("[:cardinality :name :many]", 0, 1),
        ("[:deprecate :name]", 0, 1),
        ("[:world-setting :schema :strict]", 0, 1),
    ] {
        let report: Json = serde_json::from_slice(&write(&path, source, true).stdout).unwrap();
        assert_eq!(report["changed"], changed, "{source}: {report}");
        assert_eq!(report["declared"], declared, "{source}: {report}");
        assert_eq!(report["ops"][0]["changed"], changed, "{source}: {report}");
        assert_eq!(report["ops"][0]["declared"], declared, "{source}: {report}");
    }
}

#[test]
fn ordinary_handle_and_edge_attributes_can_be_declared_or_inferred() {
    for explicit in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("world.kala");
        success(&kala(["load", VARROW, path.to_str().unwrap()]));
        if explicit {
            write(
                &path,
                "[[:attribute :handle :string] [:attribute :edge :ref]]",
                false,
            );
        }
        write(
            &path,
            "[[:assert :edwin :handle \"x\" :at 0] [:assert :edwin :edge :mara :at 0]]",
            false,
        );
        let output = kala([
            "query",
            path.to_str().unwrap(),
            "[:find ?h ?k ?other :at 0 :where [:edwin :handle ?h] [:edwin :kala_handle ?k] [:edwin :edge ?other]]",
            "--json",
        ]);
        success(&output);
        let report: Json = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(report["rows"][0][0]["value"], "x");
        assert_eq!(report["rows"][0][1]["value"], "edwin");
        assert_eq!(report["rows"][0][2]["value"]["handle"], "mara");
        // Entity descriptor and attribute namespaces remain separate on export.
        let output = kala(["export", path.to_str().unwrap(), "--at", "0"]);
        success(&output);
        let export: Json = serde_json::from_slice(&output.stdout).unwrap();
        let edwin = export["entities"]
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e["handle"] == "edwin")
            .unwrap();
        assert_eq!(edwin["handle"], "edwin");
        assert_eq!(edwin["attributes"]["handle"]["value"], "x", "{edwin}");
    }
}

#[test]
fn kala_prefix_is_refused_live_and_at_genesis() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("world.kala");
    success(&kala(["load", VARROW, path.to_str().unwrap()]));
    for source in [
        "[:attribute :kala_x :string]",
        "[:node-type :kala_T]",
        "[:create :kala_T :ann]",
        "[:assert :edwin :kala_x 1]",
        "[:rename :name :kala_name]",
        "[:attribute :parent :ref :inverse :kala_y]",
    ] {
        let before = fs::read(&path).unwrap();
        let input = path.with_extension("edn");
        fs::write(&input, source).unwrap();
        let output = kala(["transact", path.to_str().unwrap(), input.to_str().unwrap()]);
        assert_eq!(code(&output), 1, "{source}: {}", stdout(&output));
        let error = stderr(&output);
        assert!(
            error.contains("reserved") && error.contains("kala_") && error.contains("_KALA_"),
            "{error}"
        );
        assert_eq!(fs::read(&path).unwrap(), before);
    }
    for source in [
        "[:node-type :kala_T]",
        "[:attribute :kala_x :string]",
        "[:attribute :parent :ref :inverse :kala_y]",
    ] {
        let input = dir.path().join("schema.edn");
        let bad = dir.path().join("bad.kala");
        fs::write(&input, source).unwrap();
        let output = kala([
            "create",
            bad.to_str().unwrap(),
            "--schema",
            input.to_str().unwrap(),
        ]);
        assert_eq!(code(&output), 1, "{source}: {}", stdout(&output));
        assert!(stderr(&output).contains("reserved"), "{}", stderr(&output));
        assert!(!bad.exists());
    }
}

#[test]
fn edge_is_an_inverse_but_never_a_node_type() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("world.kala");
    success(&kala(["load", VARROW, path.to_str().unwrap()]));
    write(
        &path,
        "[[:attribute :mentors :ref :inverse :edge] [:assert :edwin :mentors :mara :at 0]]",
        false,
    );
    let output = kala([
        "query",
        path.to_str().unwrap(),
        "[:find ?e :at 0 :where [:mara :edge ?e]]",
        "--json",
    ]);
    success(&output);
    let report: Json = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["rows"][0][0]["value"]["handle"], "edwin");
    for source in ["[:node-type :edge]", "[:create :edge :bad]"] {
        let before = fs::read(&path).unwrap();
        let input = path.with_extension("edn");
        fs::write(&input, source).unwrap();
        let output = kala(["transact", path.to_str().unwrap(), input.to_str().unwrap()]);
        assert_eq!(code(&output), 1, "{source}: {}", stdout(&output));
        assert!(stderr(&output).contains("edge"));
        assert_eq!(fs::read(&path).unwrap(), before);
    }
}
