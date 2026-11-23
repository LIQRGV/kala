//! `kala log` shows schema entries with names, never as Rust Debug output.
mod common;

use common::*;
use serde_json::Value as Json;
use std::fs;

const DEBUG_FRAGMENTS: [&str; 9] = [
    "AttrId(",
    "EntityId(",
    "NodeTypeId(",
    "Some(",
    "None",
    "Str(",
    "AttrDef",
    "State,",
    "{ id:",
];

fn world_with_schema_history() -> Fx {
    let f = varrow();
    let input = f.dir.path().join("w.edn");
    fs::write(
        &input,
        "[[:assert :edwin :age 42 :at 0]
          [:attribute :title :string]
          [:node-type :FACTION]
          [:rename :title :honorific]
          [:cardinality :alias :one]
          [:world-setting :schema :strict]
          [:migrate :age :string]
          [:deprecate :honorific]]",
    )
    .unwrap();
    let o = f.run("transact", &[input.to_str().unwrap()]);
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    f
}

fn schema_entries(f: &Fx) -> Vec<Json> {
    let log = f.json("log", &[]);
    let tx = log["transactions"]
        .as_array()
        .unwrap()
        .last()
        .unwrap()
        .clone();
    assert_eq!(tx["tx"], 13);
    tx["entries"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["kind"] == "schema")
        .cloned()
        .collect()
}

fn change_of(entries: &[Json], pred: impl Fn(&Json) -> bool) -> &Json {
    entries
        .iter()
        .map(|e| &e["change"])
        .find(|c| pred(c))
        .unwrap_or_else(|| panic!("no matching schema entry in {entries:#?}"))
}

#[test]
fn json_schema_entries_are_structured_objects_without_debug_text() {
    let f = world_with_schema_history();
    let entries = schema_entries(&f);
    assert_eq!(entries.len(), 11, "{entries:#?}");
    for e in &entries {
        assert!(
            e["change"].is_object(),
            "`change` must be a structured object, not a Debug string: {e}"
        );
        let text = e.to_string();
        for fragment in DEBUG_FRAGMENTS {
            assert!(!text.contains(fragment), "{fragment:?} leaked into {text}");
        }
    }
}

#[test]
fn json_schema_entries_use_the_write_report_field_names() {
    let f = world_with_schema_history();
    let entries = schema_entries(&f);
    let is = |c: &Json, kind: &str, name: &str| c["kind"] == kind && c["name"] == name;

    let age = change_of(&entries, |c| {
        is(c, "attribute", "age") && c["inferred"] == true
    });
    assert_eq!(age["type"], "i64", "{age}");

    let title = change_of(&entries, |c| is(c, "attribute", "title"));
    assert_eq!(title["type"], "string");
    assert_eq!(title["inferred"], false);

    let faction = change_of(&entries, |c| is(c, "node_type", "FACTION"));
    assert_eq!(faction["inferred"], false);

    // A rename logs the full new definition under its new name.
    change_of(&entries, |c| {
        is(c, "attribute", "honorific") && c["deprecated"] == false
    });
    // The deprecation logs the same attribute with the flag set.
    change_of(&entries, |c| {
        is(c, "attribute", "honorific") && c["deprecated"] == true
    });
    // The cardinality edit shows the new label.
    let alias = change_of(&entries, |c| is(c, "attribute", "alias"));
    assert_eq!(alias["cardinality"], "one", "{alias}");

    let setting = change_of(&entries, |c| c["kind"] == "world_setting");
    assert_eq!(setting["key"], "schema");
    assert_eq!(setting["value"], "strict");

    let migrate = change_of(&entries, |c| c["kind"] == "migrate");
    assert_eq!(migrate["name"], "age", "{migrate}");
    assert_eq!(migrate["to"], "string", "{migrate}");

    // The migration's own rename/deprecate of the old attribute.
    change_of(&entries, |c| {
        is(c, "attribute", "age@old") && c["deprecated"] == true
    });
}

#[test]
fn text_log_shows_names_and_no_debug_fragments_for_schema_entries() {
    let f = world_with_schema_history();
    let out = f.text("log", &[]);
    let out = out
        .split("tx#13")
        .nth(1)
        .expect("tx#13 in the log")
        .to_string();
    let lines: Vec<&str> = out.lines().filter(|l| l.contains(" schema ")).collect();
    assert_eq!(lines.len(), 11, "{out}");
    for line in &lines {
        for fragment in DEBUG_FRAGMENTS {
            assert!(!line.contains(fragment), "{fragment:?} in {line}");
        }
    }
    let joined = lines.join("\n");
    for name in [
        "age",
        "title",
        "honorific",
        "alias",
        "FACTION",
        "strict",
        "age@old",
    ] {
        assert!(joined.contains(name), "{name} missing from\n{joined}");
    }
    assert!(joined.contains("i64"), "{joined}");
    assert!(joined.contains("string"), "{joined}");
}
