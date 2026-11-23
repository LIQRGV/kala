//! Total story-check output is bounded without losing exact check counts.
mod common;

use common::{code, kala, parse_one_json, stderr, stdout};
use kala_core::{AttrDef, Cardinality, Kind, Schema, Tick, Value, ValueType, World};
use std::path::{Path, PathBuf};

const ROW_CAP: usize = 10_000;
const BYTE_CAP: usize = 16 * 1024 * 1024;
const QUERY: &str = "[:find ?v :where [:e :value ?v]]";

fn world(dir: &Path, values: Vec<Value>) -> PathBuf {
    let mut attr = AttrDef::new(
        "value",
        if matches!(values[0], Value::Str(_)) {
            ValueType::Str
        } else {
            ValueType::I64
        },
        Kind::State,
        Cardinality::Many,
    );
    attr.indexed = false;
    let path = dir.join("w.kala");
    let mut world = World::create(&path, Schema::new().node_type("THING").attribute(attr)).unwrap();
    let entity = world.create_entity("THING", Some("e")).unwrap();
    for value in values {
        world.assert(entity, "value", value, Some(Tick(0))).unwrap();
    }
    path
}

fn run(dir: &Path, world: &Path, count: usize, json: bool) -> String {
    let checks = dir.join("checks.edn");
    let mut source = String::from("[\n");
    for i in 0..count {
        source.push_str(&format!("[:check{i} {QUERY}]\n"));
    }
    source.push_str("]\n");
    std::fs::write(&checks, source).unwrap();
    let mut args = vec![
        "doctor".as_ref(),
        world.as_os_str(),
        "--checks".as_ref(),
        checks.as_os_str(),
    ];
    if json {
        args.push("--json".as_ref());
    }
    let result = kala(args);
    assert_eq!(code(&result), 0, "{}", stderr(&result));
    stdout(&result)
}

#[test]
fn json_total_row_cap_preserves_all_counts_and_reports_one_truncation() {
    let dir = tempfile::tempdir().unwrap();
    let path = world(dir.path(), (0..100).map(Value::I64).collect());
    let out = parse_one_json(&run(dir.path(), &path, 103, true));
    let hints = out["hints"].as_array().unwrap();
    let checks: Vec<_> = hints
        .iter()
        .filter(|h| h["kind"] == "story_check")
        .collect();
    assert_eq!(checks.len(), 103);
    let rendered: usize = checks
        .iter()
        .map(|h| h["rows"].as_array().unwrap().len())
        .sum();
    assert_eq!(
        rendered, ROW_CAP,
        "total rendered rows must stop at the global cap"
    );
    for (i, check) in checks.iter().enumerate() {
        assert_eq!(check["check"], format!("check{i}"));
        assert_eq!(check["rows_total"], 100);
        assert_eq!(
            check["rows"].as_array().unwrap().len(),
            if i < 100 { 100 } else { 0 }
        );
    }
    let truncated: Vec<_> = hints
        .iter()
        .filter(|h| h["kind"] == "output_truncated")
        .collect();
    assert_eq!(truncated.len(), 1);
    assert_eq!(truncated[0]["check"], "check100");
}

#[test]
fn the_global_row_cap_can_cut_partway_through_a_check() {
    let dir = tempfile::tempdir().unwrap();
    let path = world(dir.path(), (0..99).map(Value::I64).collect());
    let out = parse_one_json(&run(dir.path(), &path, 103, true));
    let hints = out["hints"].as_array().unwrap();
    let checks: Vec<_> = hints
        .iter()
        .filter(|h| h["kind"] == "story_check")
        .collect();
    assert_eq!(checks.len(), 103);
    assert!(checks.iter().all(|h| h["rows_total"] == 99));
    assert_eq!(
        checks[..101]
            .iter()
            .map(|h| h["rows"].as_array().unwrap().len())
            .sum::<usize>(),
        9_999
    );
    assert_eq!(checks[101]["rows"].as_array().unwrap().len(), 1);
    assert!(checks[102]["rows"].as_array().unwrap().is_empty());
    let truncated: Vec<_> = hints
        .iter()
        .filter(|h| h["kind"] == "output_truncated")
        .collect();
    assert_eq!(truncated.len(), 1);
    assert_eq!(truncated[0]["check"], "check101");
}

#[test]
fn text_at_the_check_limit_preserves_all_rows_and_counts() {
    let dir = tempfile::tempdir().unwrap();
    let path = world(dir.path(), (0..5).map(Value::I64).collect());
    // MAX_CHECKS is 256, so 5 text rows/check cannot reach the global
    // 10,000-row cap. The byte-cap cases below exercise text truncation.
    let out = run(dir.path(), &path, 256, false);
    let rows: Vec<_> = out
        .lines()
        .filter(|line| line.starts_with("    ") && !line.starts_with("    ..."))
        .collect();
    assert_eq!(rows.len(), 256 * 5);
    assert!(rows.len() <= ROW_CAP);
    assert_eq!(out.matches("  story_check:").count(), 256);
    assert!(!out.contains("output_truncated"));
    for i in 0..256 {
        assert!(out.contains(&format!("  story_check: :check{i}: 5 rows\n")));
    }
}

#[test]
fn wide_rows_obey_the_total_byte_cap_in_text_and_json() {
    let dir = tempfile::tempdir().unwrap();
    // An unindexed value is legal at this size. Escaping must count toward
    // the output cap too (C1 CSI uses six ASCII bytes in both renderers).
    let value = "\u{009b}".repeat(128 * 1024);
    let path = world(dir.path(), vec![Value::Str(value)]);
    for json in [true, false] {
        let output = run(dir.path(), &path, 30, json);
        if json {
            let out = parse_one_json(&output);
            let hints = out["hints"].as_array().unwrap();
            let checks: Vec<_> = hints
                .iter()
                .filter(|h| h["kind"] == "story_check")
                .collect();
            assert_eq!(checks.len(), 30);
            assert!(checks.iter().all(|h| h["rows_total"] == 1));
            // Count the escaped bytes actually emitted, independently of
            // serde_json's unescaped in-memory string representation.
            assert!(output.matches("\\u009b").count() * 6 <= BYTE_CAP);
            let truncations: Vec<_> = hints
                .iter()
                .filter(|h| h["kind"] == "output_truncated")
                .collect();
            assert_eq!(truncations.len(), 1);
            let first_empty = checks
                .iter()
                .position(|h| h["rows"].as_array().unwrap().is_empty())
                .unwrap();
            assert!(first_empty > 0 && first_empty < 30);
            assert_eq!(truncations[0]["check"], format!("check{first_empty}"));
            assert!(
                checks[first_empty..]
                    .iter()
                    .all(|h| h["rows"].as_array().unwrap().is_empty())
            );
        } else {
            assert_eq!(output.matches("  story_check:").count(), 30);
            let rows: Vec<_> = output
                .lines()
                .filter(|line| line.starts_with("    ") && !line.starts_with("    ..."))
                .collect();
            let bytes: usize = rows.iter().map(|line| line.len() + 1).sum();
            assert!(bytes <= BYTE_CAP, "rendered row text used {bytes} bytes");
            assert!(!rows.is_empty() && rows.len() < 30);
            assert_eq!(output.matches("output_truncated").count(), 1);
        }
    }
}

#[test]
fn a_single_oversize_row_is_omitted_and_later_counts_stay_exact() {
    let dir = tempfile::tempdir().unwrap();
    // Escapes expand this 6 MiB value beyond 16 MiB; the query itself fits
    // the normal execution memory budget.
    let path = world(
        dir.path(),
        vec![Value::Str("\u{009b}".repeat(3 * 1024 * 1024))],
    );
    for json in [true, false] {
        let output = run(dir.path(), &path, 2, json);
        assert!(output.len() < 4096, "oversize row must never be retained");
        if json {
            let out = parse_one_json(&output);
            let hints = out["hints"].as_array().unwrap();
            let checks: Vec<_> = hints
                .iter()
                .filter(|h| h["kind"] == "story_check")
                .collect();
            assert_eq!(checks.len(), 2);
            assert!(
                checks
                    .iter()
                    .all(|h| h["rows_total"] == 1 && h["rows"].as_array().unwrap().is_empty())
            );
            let truncated: Vec<_> = hints
                .iter()
                .filter(|h| h["kind"] == "output_truncated")
                .collect();
            assert_eq!(truncated.len(), 1);
            assert_eq!(truncated[0]["check"], "check0");
        } else {
            assert_eq!(output.matches("  story_check:").count(), 2);
            assert_eq!(output.matches("output_truncated").count(), 1);
            assert!(output.contains("    ... (1 more)"));
        }
    }
}
