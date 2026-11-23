//! Snapshot exports expose typed values at a consistent tick and canon.
mod common;
use common::{Fx, code, kala, stderr, varrow};
use serde_json::Value as Json;
use std::fs;

fn export(f: &Fx, args: &[&str]) -> Json {
    let o = f.run("export", args);
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    serde_json::from_slice(&o.stdout).unwrap()
}
fn entity<'a>(snapshot: &'a Json, handle: &str) -> &'a Json {
    snapshot["entities"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["handle"] == handle)
        .unwrap()
}
fn allies(snapshot: &Json) -> Vec<&str> {
    entity(snapshot, "edwin")["attributes"]["ally_of"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v["value"]["handle"].as_str().unwrap())
        .collect()
}

#[test]
fn varrow_snapshot_contains_typed_entities_schema_and_holding_edges() {
    let f = varrow();
    let j = export(&f, &["--at", "-208"]);
    assert_eq!(j["tick"], -208);
    assert_eq!(j["canon"], 12);
    assert_eq!(j["format_version"], kala_core::log::LOG_FORMAT_VERSION);
    assert!(j["kala_version"].is_string());
    assert_eq!(j["schema"]["node_types"].as_array().unwrap().len(), 4);
    assert_eq!(j["schema"]["attributes"].as_array().unwrap().len(), 11);
    assert_eq!(entity(&j, "edwin")["node_type"], "CHARACTER");
    assert_eq!(entity(&j, "edwin")["attributes"]["name"]["type"], "string");
    assert_eq!(entity(&j, "edwin")["attributes"]["name"]["value"], "Edwin");
    assert_eq!(allies(&j), ["greg", "mara", "tomas"]);
    let past = export(&f, &["--at", "-240"]);
    let edge = past["edges"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| {
            e["source"]["handle"] == "edwin"
                && e["attribute"] == "present"
                && e["target"]["handle"] == "carnath"
        })
        .unwrap();
    assert_eq!(edge["properties"]["disguised"]["type"], "bool");
    assert_eq!(edge["properties"]["disguised"]["value"], true);
    let after = export(&f, &["--at", "700"]);
    assert!(
        !after["edges"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["id"] == edge["id"])
    );
}

#[test]
fn retcon_and_default_tick_respect_canon_and_old_definitions_use_latest_names() {
    let f = varrow();
    assert_eq!(
        allies(&export(&f, &["--at", "-195", "--canon", "tx#10"])),
        ["greg", "mara", "tomas"]
    );
    assert_eq!(allies(&export(&f, &["--at", "-195"])), ["mara", "tomas"]);
    let default = export(&f, &[]);
    assert_eq!(default["tick"], 700);
    let input = f.dir.path().join("rename.edn");
    fs::write(
        &input,
        "[[:rename :located_in :location] [:cardinality :ally_of :one] [:deprecate :alias]]",
    )
    .unwrap();
    let o = kala([
        "transact",
        f.path.to_str().unwrap(),
        input.to_str().unwrap(),
    ]);
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    let old = export(&f, &["--at", "-208", "--canon", "tx#12"]);
    assert!(
        old["schema"]["attributes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|a| a["name"] == "location")
    );
    let old_allies = old["schema"]["attributes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["name"] == "ally_of")
        .unwrap();
    assert_eq!(old_allies["cardinality"], "many");
    let now = export(&f, &["--at", "-208"]);
    let now_allies = now["schema"]["attributes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["name"] == "ally_of")
        .unwrap();
    assert_eq!(now_allies["cardinality"], "one");
    assert_eq!(
        allies(&now),
        ["greg", "mara", "tomas"],
        "overlapping cardinality-one values must remain an array"
    );
}

#[test]
fn export_is_deterministic_read_only_and_can_write_a_file() {
    let f = varrow();
    let before = fs::read(&f.path).unwrap();
    let locked = kala_core::World::open(&f.path).unwrap();
    let first = f.run("export", &["--at", "-208"]);
    assert_eq!(code(&first), 0, "{}", stderr(&first));
    let second = f.run("export", &["--at", "-208"]);
    assert_eq!(first.stdout, second.stdout);
    let path = f.dir.path().join("snapshot.json");
    let output = f.run("export", &["--at", "-208", "-o", path.to_str().unwrap()]);
    assert_eq!(code(&output), 0, "{}", stderr(&output));
    assert_eq!(fs::read(path).unwrap(), first.stdout);
    assert_eq!(fs::read(&f.path).unwrap(), before);
    drop(locked);
}

#[test]
fn export_refuses_accounts_even_when_the_system_file_is_renamed() {
    let dir = tempfile::tempdir().unwrap();
    kala_bolt::system::add(dir.path(), "alice@localhost", None).unwrap();
    let renamed = dir.path().join("ordinary.kala");
    fs::rename(dir.path().join("system.kala"), &renamed).unwrap();
    let o = kala(["export", renamed.to_str().unwrap()]);
    assert_ne!(code(&o), 0);
    assert!(stderr(&o).contains("system"), "{}", stderr(&o));
    assert!(o.stdout.is_empty());
}
