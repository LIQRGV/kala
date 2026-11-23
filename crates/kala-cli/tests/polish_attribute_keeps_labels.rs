//! `[:attribute NAME TYPE]` without options keeps what the attribute already
//! has; only explicit options change labels.
mod common;

use common::*;
use serde_json::Value as Json;
use std::fs;

fn transact(f: &Fx, source: &str) -> std::process::Output {
    let input = f.dir.path().join("w.edn");
    fs::write(&input, source).unwrap();
    f.run("transact", &[input.to_str().unwrap()])
}

fn write(f: &Fx, source: &str) -> String {
    let o = transact(f, source);
    assert_eq!(code(&o), 0, "{source}: {}", stderr(&o));
    stdout(&o)
}

fn def(f: &Fx, name: &str) -> Json {
    let schema = f.json("schema", &[]);
    schema["attributes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["name"] == name)
        .unwrap_or_else(|| panic!("no attribute {name}: {schema}"))
        .clone()
}

/// Everything except the origin flag.
fn labels(d: &Json) -> Json {
    let mut d = d.clone();
    d.as_object_mut().unwrap().remove("inferred");
    d
}

#[test]
fn bare_attribute_form_keeps_inferred_cardinality_and_only_marks_it_declared() {
    let f = varrow();
    write(&f, "[:assert :edwin :owns :tomas :at 0]");
    let before = def(&f, "owns");
    assert_eq!(before["cardinality"], "many");
    assert_eq!(before["inferred"], true);
    let report = write(&f, "[:attribute :owns :ref]");
    let after = def(&f, "owns");
    assert_eq!(after["inferred"], false, "{after}");
    assert_eq!(
        after["cardinality"], "many",
        "a bare redeclaration must not silently turn many into one: {after}"
    );
    assert_eq!(labels(&after), labels(&before));
    assert!(!report.contains("cardinality of"), "{report}");
}

#[test]
fn bare_attribute_form_keeps_inverse_and_reverse_labels() {
    let f = varrow();
    write(
        &f,
        "[:attribute :parent_of :ref :many :inverse :child_of :reverse :one]",
    );
    let before = def(&f, "parent_of");
    assert_eq!(before["inverse"], "child_of");
    assert_eq!(before["reverse_cardinality"], "one");
    write(&f, "[:attribute :parent_of :ref]");
    let after = def(&f, "parent_of");
    assert_eq!(after["inverse"], "child_of", "{after}");
    assert_eq!(after["reverse_cardinality"], "one", "{after}");
    assert_eq!(after["cardinality"], "many", "{after}");
    // The fixture's own `ruled_by` (cardinality one, reverse one) as well.
    let before = def(&f, "ruled_by");
    write(&f, "[:attribute :ruled_by :ref]");
    assert_eq!(def(&f, "ruled_by"), before);
}

#[test]
fn explicit_cardinality_option_changes_it_and_the_report_says_so() {
    let f = varrow();
    write(&f, "[:assert :edwin :owns :tomas :at 0]");
    let report = write(&f, "[:attribute :owns :ref :one]");
    assert!(
        report.contains("cardinality of :owns: many -> one"),
        "{report}"
    );
    assert_eq!(def(&f, "owns")["cardinality"], "one");
}
