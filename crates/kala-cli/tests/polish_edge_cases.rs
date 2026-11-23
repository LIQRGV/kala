//! Edge cases for the PR #6 polish fixes, through the `kala` binary.
mod common;

use common::*;
use serde_json::Value as Json;
use std::fs;

fn transact(f: &Fx, source: &str) -> std::process::Output {
    let input = f.dir.path().join("w.edn");
    fs::write(&input, source).unwrap();
    f.run("transact", &[input.to_str().unwrap()])
}

fn ok(f: &Fx, source: &str) -> String {
    let o = transact(f, source);
    assert_eq!(code(&o), 0, "{source}: {}", stderr(&o));
    stdout(&o)
}

fn last_tx(f: &Fx) -> Json {
    f.json("log", &[])["transactions"]
        .as_array()
        .unwrap()
        .last()
        .unwrap()
        .clone()
}

fn schema_changes(tx: &Json) -> Vec<Json> {
    tx["entries"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["kind"] == "schema")
        .map(|e| e["change"].clone())
        .collect()
}

fn doctor_clean(f: &Fx) {
    let o = f.run("doctor", &[]);
    assert_eq!(code(&o), 0, "{}{}", stdout(&o), stderr(&o));
}

// --- 4 ---------------------------------------------------------------------

#[test]
fn reserved_inverse_then_unknown_retract_reports_the_reserved_refusal_and_writes_nothing() {
    let f = varrow();
    let before = fs::read(&f.path).unwrap();
    let o = transact(
        &f,
        "[[:attribute :kin :ref :inverse :_KALA_kin] [:retract :edwin :nope 1 :at 0]]",
    );
    assert_ne!(code(&o), 0, "{}", stdout(&o));
    let err = stderr(&o);
    assert!(
        err.contains("_KALA_") && err.to_lowercase().contains("reserved"),
        "{err}"
    );
    no_raw_controls(&err);
    assert_eq!(fs::read(&f.path).unwrap(), before);
}

#[test]
fn lowercase_kala_prefix_is_allowed() {
    let f = varrow();
    ok(&f, "[[:attribute :_kala_x :string] [:node-type :_kala_t]]");
    doctor_clean(&f);
}

// --- 5 ---------------------------------------------------------------------

#[test]
fn attribute_form_on_inferred_attribute_logs_two_entries_and_reports_two_lines() {
    let f = varrow();
    ok(&f, "[:assert :edwin :age 42 :at 0]");
    let out = ok(&f, "[:attribute :age :i64 :many]");
    assert!(out.contains("declared :age as i64 (declared)"), "{out}");
    assert!(out.contains("cardinality of :age: one -> many"), "{out}");
    let changes = schema_changes(&last_tx(&f));
    assert_eq!(changes.len(), 2, "{changes:#?}");
    assert_eq!(changes[0]["inferred"], false);
    assert_eq!(changes[1]["cardinality"], "many");
    doctor_clean(&f);
    // Reopen: a later read of the world sees the label.
    let schema = f.text("schema", &[]);
    assert!(schema.contains("age"), "{schema}");
}

#[test]
fn attribute_form_on_deprecated_attribute_is_refused_naming_deprecation() {
    let f = varrow();
    ok(&f, "[[:attribute :z :string] [:deprecate :z]]");
    let before = fs::read(&f.path).unwrap();
    let o = transact(&f, "[:attribute :z :string]");
    assert_eq!(code(&o), 1, "{}", stdout(&o));
    assert!(
        stderr(&o).to_lowercase().contains("deprecated"),
        "{}",
        stderr(&o)
    );
    assert_eq!(fs::read(&f.path).unwrap(), before);
}

#[test]
fn attribute_form_with_new_type_on_factless_attribute_starts_from_defaults() {
    let f = varrow();
    ok(&f, "[:attribute :z :string :many :event]");
    ok(&f, "[:attribute :z :i64]");
    // Final definition (the last schema entry of the transaction).
    let changes = schema_changes(&last_tx(&f));
    let c = changes.last().unwrap();
    assert_eq!(c["type"], "i64", "{c}");
    assert_eq!(c["cardinality"], "one", "{c}");
    assert_eq!(c["attribute_kind"], "state", "{c}");
    doctor_clean(&f);
}

#[test]
fn type_change_on_factless_attribute_is_one_redefinition() {
    let f = varrow();
    ok(&f, "[:attribute :z :string :many :event]");
    let out = ok(&f, "[:attribute :z :i64]");
    let changes = schema_changes(&last_tx(&f));
    assert_eq!(
        changes.len(),
        1,
        "one schema entry for the form: {changes:?}"
    );
    assert_eq!(changes[0]["type"], "i64", "{}", changes[0]);
    assert_eq!(changes[0]["cardinality"], "one", "{}", changes[0]);
    assert_eq!(changes[0]["attribute_kind"], "state", "{}", changes[0]);
    assert!(
        !out.contains("cardinality of :z"),
        "no spurious cardinality line: {out}"
    );
    doctor_clean(&f);
}

// --- 6 ---------------------------------------------------------------------

fn assert_clean_error(err: &str) {
    no_raw_controls(err);
    for b in err.trim_end_matches('\n').bytes() {
        assert!(b >= 0x20 && b != 0x7f, "raw control byte {b:#x} in {err:?}");
    }
    assert!(
        !err.contains('\u{2028}') && !err.contains('\u{202e}'),
        "{err:?}"
    );
}

#[test]
fn unconvertible_value_with_newline_and_escape_is_escaped_in_the_message() {
    let f = varrow();
    ok(
        &f,
        "[:assert :tomas :x \"line1\\nline2\\u001b[31m\\ttab\" :at 0]",
    );
    let shown = stdout(&f.run("asof", &["tomas", "x", "--at", "0"]));
    no_raw_controls(&shown);
    let o = transact(&f, "[:migrate :x :i64]");
    assert_eq!(code(&o), 1, "{}", stdout(&o));
    let err = stderr(&o);
    assert_clean_error(&err);
    assert!(err.contains("unconvertible"), "{err}");
    assert!(err.contains("\\n"), "newline shown escaped: {err}");
    assert!(err.contains("tomas"), "{err}");
}

#[test]
fn unconvertible_value_over_200_chars_is_truncated_in_the_message() {
    let f = varrow();
    let long = "q".repeat(300);
    ok(&f, "[:attribute :x :string :unindexed]");
    ok(&f, &format!("[:assert :tomas :x \"{long}\" :at 0]"));
    let o = transact(&f, "[:migrate :x :i64]");
    assert_eq!(code(&o), 1, "{}", stdout(&o));
    let err = stderr(&o);
    assert_clean_error(&err);
    assert!(
        !err.contains(&"q".repeat(201)),
        "not truncated ({} bytes): {err}",
        err.len()
    );
    assert!(
        err.contains(&format!("{}\"...", "q".repeat(200))),
        "cut at 200 chars then ...: {err}"
    );
    assert!(err.len() < 600, "{} bytes: {err}", err.len());
}

// --- 7 ---------------------------------------------------------------------

const DEBUG_FRAGMENTS: [&str; 8] = [
    "AttrId(",
    "EntityId(",
    "Some(",
    "None",
    "Str(",
    "AttrDef",
    "State,",
    "{ id:",
];

#[test]
fn log_json_for_skip_unconvertible_migration_is_structured() {
    let f = varrow();
    ok(
        &f,
        "[[:assert :edwin :x \"1\" :at 0] [:assert :tomas :x \"zz\" :at 0]]",
    );
    let out = ok(&f, "[:migrate :x :i64 :skip-unconvertible]");
    assert!(
        out.contains("1 skipped") || out.contains("skipped"),
        "{out}"
    );
    let tx = last_tx(&f);
    let text = tx.to_string();
    for fragment in DEBUG_FRAGMENTS {
        assert!(!text.contains(fragment), "{fragment:?} in {text}");
    }
    let changes = schema_changes(&tx);
    let m = changes
        .iter()
        .find(|c| c["kind"] == "migrate")
        .expect("migrate entry");
    assert_eq!(m["skip_unconvertible"], true, "{m}");
    assert_eq!(m["to"], "i64");
    assert_eq!(m["from"], "string");
    assert_eq!(m["name"], "x");
    assert_eq!(m["source"], "x@old", "{m}");
    // The plain-text log is valid and mentions the flag.
    let txt = f.text("log", &[]);
    assert!(txt.contains("skipping unconvertible"), "{txt}");
    doctor_clean(&f);
}

#[test]
fn log_json_migrate_source_shows_the_latest_name_after_a_rename() {
    let f = varrow();
    ok(&f, "[:assert :edwin :x \"1\" :at 0]");
    ok(&f, "[:migrate :x :i64]");
    ok(&f, "[:rename :x@old :x_v1]");
    // Whole document is one valid JSON value (f.json parses it).
    let log = f.json("log", &[]);
    let text = log.to_string();
    for fragment in DEBUG_FRAGMENTS {
        assert!(!text.contains(fragment), "{fragment:?} in {text}");
    }
    let mig_tx = log["transactions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| schema_changes(t).iter().any(|c| c["kind"] == "migrate"))
        .expect("migration tx");
    let m = schema_changes(mig_tx)
        .into_iter()
        .find(|c| c["kind"] == "migrate")
        .unwrap();
    assert_eq!(m["source"], "x_v1", "latest name expected: {m}");
    let txt = f.text("log", &[]);
    assert!(txt.contains("from :x_v1"), "{txt}");
    assert!(!txt.contains("from :x@old"), "{txt}");
    doctor_clean(&f);
}

// --- 8 ---------------------------------------------------------------------

#[test]
fn migrating_parent_of_with_inverse_and_edge_properties_replays_and_doctor_is_clean() {
    let f = varrow();
    ok(
        &f,
        "[[:attribute :parent_of :ref :many :inverse :child_of :reverse :one]
          [:assert :edwin :parent_of :tomas :at 0]]",
    );
    let before_ids = f.text("log", &[]);
    assert!(before_ids.contains("parent_of"));
    let out = ok(&f, "[:migrate :parent_of :bidirectional]");
    assert!(out.contains("child_of") && out.contains("dropped"), "{out}");
    // Reopen twice: every command replays the log and must agree.
    let a = f.text("log", &[]);
    let b = f.text("log", &[]);
    assert_eq!(a, b);
    doctor_clean(&f);
    let o = f.run("asof", &["tomas", "parent_of", "--at", "0"]);
    assert_eq!(code(&o), 0, "{}", stderr(&o));
}
