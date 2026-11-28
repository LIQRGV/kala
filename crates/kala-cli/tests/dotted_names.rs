#[path = "../../kala-lang/tests/common/dotted_names.rs"]
mod cases;
mod common;

use common::{code, kala, stderr, varrow};
use std::fs;
use std::process::Output;

fn refusal(output: &Output, name: &str) {
    assert_ne!(code(output), 0, "accepted dotted name {name}");
    let text = stderr(output);
    assert!(
        text.contains(name) && text.contains("may not contain '.'"),
        "{text}"
    );
}

#[test]
fn dotted_names_through_transact_leave_the_log_untouched() {
    let fx = varrow();
    let before = fs::read(&fx.path).unwrap();
    let source = fx.dir.path().join("input");
    for &(lang, name, query) in cases::WRITES {
        fs::write(&source, query).unwrap();
        refusal(
            &fx.run("transact", &["--lang", lang, source.to_str().unwrap()]),
            name,
        );
        assert!(fs::read(&fx.path).unwrap() == before, "{query}");
    }
}

#[test]
fn dotted_name_read_mentions_keep_each_languages_missing_property_behavior() {
    let fx = varrow();
    let output = fx.run("query", &[cases::DATALOG_READ]);
    assert_ne!(code(&output), 0);
    let text = stderr(&output);
    assert!(text.contains("unknown attribute `:a.b`"), "{text}");
    let output = fx.run("query", &["--lang", "cypher", cases::CYPHER_READ, "--json"]);
    assert_eq!(code(&output), 0, "{}", stderr(&output));
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(
        json["rows"],
        serde_json::json!([[{"type":"null","value":null}]])
    );
}

#[test]
fn dotted_names_in_schema_files_leave_no_world() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("world.kala");
    let schema = dir.path().join("schema.edn");
    for (name, form) in [
        ("a.b", "[:attribute :a.b :string]"),
        ("N.PC", "[:node-type :N.PC]"),
        (
            "carried.by",
            "[:attribute :carries :ref :inverse :carried.by]",
        ),
    ] {
        fs::write(&schema, form).unwrap();
        refusal(
            &kala([
                "create",
                path.to_str().unwrap(),
                "--schema",
                schema.to_str().unwrap(),
            ]),
            name,
        );
        assert!(!path.exists(), "{form}");
    }
}

#[test]
fn dotted_names_in_load_preserve_the_destination() {
    let fx = varrow();
    let before = fs::read(&fx.path).unwrap();
    let source = fx.dir.path().join("bad.yaml");
    fs::write(
        &source,
        "schema: {node_types: [NPC]}\nentities: {NPC: [durn, npc.durn]}\n",
    )
    .unwrap();
    refusal(
        &kala([
            "load",
            source.to_str().unwrap(),
            fx.path.to_str().unwrap(),
            "--force",
        ]),
        "npc.durn",
    );
    assert!(fs::read(&fx.path).unwrap() == before);
}
