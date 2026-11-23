//! Slice 6 item 7, CONFIRM edge cases for `kala query` / `kala explain`.

mod common;

use common::*;
use kala_core::load_fixture;
use serde_json::Value as Json;
use std::ffi::OsString;
use std::path::PathBuf;

const ALLIES: &str = "[:find ?a :in $ ?who :where [?e :kala_handle ?who] [?e :ally_of ?a]]";
const ALLIES_AT: &str =
    "[:find ?a :in $ ?who ?t ?c :at ?t :canon ?c :where [?e :kala_handle ?who] [?e :ally_of ?a]]";

struct W {
    _dir: tempfile::TempDir,
    dir: PathBuf,
    path: PathBuf,
}

fn load_text(yaml: &str) -> W {
    let dir = tempfile::tempdir().unwrap();
    let y = dir.path().join("f.yaml");
    std::fs::write(&y, yaml).unwrap();
    let path = dir.path().join("w.kala");
    let o = kala([OsString::from("load"), y.into(), path.clone().into()]);
    assert_eq!(code(&o), 0, "kala load: {}", stderr(&o));
    W {
        dir: dir.path().to_path_buf(),
        _dir: dir,
        path,
    }
}

fn varrow() -> W {
    load_text(&std::fs::read_to_string(VARROW).unwrap())
}

impl W {
    fn run(&self, cmd: &str, rest: &[&str]) -> std::process::Output {
        let mut a: Vec<OsString> = vec![cmd.into(), self.path.clone().into()];
        a.extend(rest.iter().map(|s| OsString::from(*s)));
        kala(a)
    }
    fn q(&self, rest: &[&str]) -> std::process::Output {
        self.run("query", rest)
    }
}

fn assert_user_error(o: &std::process::Output, what: &str) -> String {
    assert_eq!(code(o), 1, "{what}: stderr {}", stderr(o));
    assert_eq!(stdout(o), "", "{what}: stdout must be empty");
    let e = stderr(o);
    assert!(e.starts_with("kala: error: "), "{what}: {e}");
    e
}

fn big_world(n: usize) -> W {
    let handles: Vec<String> = (0..n).map(|i| format!("c{i}")).collect();
    let mut y = format!(
        "schema:\n  node_types: [CHARACTER]\n  attributes:\n    - {{ name: name, type: string }}\nentities:\n  CHARACTER: [{}]\ntransactions:\n  - tick: 1\n    ops:\n",
        handles.join(", ")
    );
    for h in &handles {
        y.push_str(&format!("      - assert: [{h}, name, \"N{h}\"]\n"));
    }
    load_text(&y)
}

#[test]
fn row_limit_is_exit_1_with_clear_message_and_empty_stdout() {
    let w = big_world(60); // 60^3 = 216_000 > 100_000
    let q = "[:find ?a ?b ?c :where [?a :name ?x] [?b :name ?y] [?c :name ?z]]";
    let o = w.q(&[q]);
    let e = assert_user_error(&o, "row limit");
    assert!(
        e.contains("100000") || e.contains("100_000"),
        "limit not named: {e}"
    );
    assert!(e.contains("rows") || e.contains("steps"), "unclear: {e}");
    no_raw_controls(&e);
    let o = w.q(&[q, "--json"]);
    assert_user_error(&o, "row limit --json");
}

#[test]
fn step_budget_or_row_limit_on_larger_cartesian_is_exit_1() {
    // 4-way Cartesian of 60: the row limit trips long before the end.
    let w = big_world(60);
    let q = "[:find ?a :where [?a :name ?x] [?b :name ?y] [?c :name ?z] [?d :name ?w]]";
    assert_user_error(&w.q(&[q]), "4-way cartesian");
}

#[test]
fn query_file_not_utf8_is_exit_1() {
    let w = varrow();
    let f = w.dir.join("bad.edn");
    std::fs::write(&f, [b'[', b':', 0xff, 0xfe, b']']).unwrap();
    let o = w.q(&["-f", f.to_str().unwrap()]);
    let e = assert_user_error(&o, "non-utf8 file");
    assert!(e.contains("UTF-8"), "{e}");
    let o = w.run("explain", &["-f", f.to_str().unwrap()]);
    assert_user_error(&o, "explain non-utf8 file");
}

#[test]
fn canon_param_above_i64_max_is_exit_1() {
    let w = varrow();
    let q = "[:find ?a :in $ ?e ?c :canon ?c :where [?e :ally_of ?a]]";
    let o = w.q(&[q, "--param", "e=edwin", "--param", "c=18446744073709551615"]);
    assert_user_error(&o, "canon u64::MAX");
    let o = w.q(&[q, "--param", "e=edwin", "--param", "c=9223372036854775808"]);
    assert_user_error(&o, "canon i64::MAX+1");
}

#[test]
fn empty_param_name_is_unknown_param_exit_1() {
    let w = varrow();
    let o = w.q(&[ALLIES, "--param", "who=edwin", "--param", "=x"]);
    let e = assert_user_error(&o, "empty name");
    assert!(e.contains("unknown parameter"), "{e}");
}

#[test]
fn undefined_edge_param_is_exit_1() {
    let w = varrow();
    let q = "[:find ?a :in $ ?e :where [?e :ally_of ?a]]";
    assert_user_error(&w.q(&[q, "--param", "e=edge#999999"]), "undefined edge");
    assert_user_error(
        &w.run("explain", &[q, "--param", "e=edge#999999"]),
        "explain undefined edge",
    );
}

#[test]
fn explain_with_undefined_entity_id_param_is_exit_1() {
    let w = varrow();
    let q = "[:find ?a :in $ ?e :where [?e :ally_of ?a]]";
    assert_user_error(
        &w.run("explain", &[q, "--param", "e=CHARACTER#9999"]),
        "explain undefined id",
    );
}

#[test]
fn ref_value_param_by_handle_equals_by_id() {
    let w = varrow();
    let q = "[:find ?e :in $ ?v :where [?e :ally_of ?v]]";
    let by_handle = w.q(&[q, "--param", "v=mara"]);
    assert_eq!(code(&by_handle), 0, "{}", stderr(&by_handle));
    let id = {
        let o = w.run("asof", &["mara", "--json"]);
        let j: Json = serde_json::from_str(stdout(&o).trim()).unwrap();
        j["entity"]["id"].as_str().unwrap().to_string()
    };
    let by_id = w.q(&[q, "--param", &format!("v={id}")]);
    assert_eq!(code(&by_id), 0, "{}", stderr(&by_id));
    assert_eq!(stdout(&by_handle), stdout(&by_id));
    assert!(
        !stdout(&by_handle).contains("(no rows)"),
        "{}",
        stdout(&by_handle)
    );
}

#[test]
fn explain_matches_library_plan_exactly() {
    let w = varrow();
    let o = w.run("explain", &[ALLIES, "--param", "who=edwin"]);
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    let twin = load_fixture(&std::fs::read_to_string(VARROW).unwrap()).unwrap();
    let p = kala_datalog::prepare(&twin, ALLIES).unwrap();
    assert_eq!(stdout(&o), p.plan.explain(&twin));
}

#[test]
fn json_rows_escape_controls_and_round_trip() {
    let w = load_text(&ctrl_fixture());
    let q = "[:find ?n :where [?e :name ?n]]";
    let o = w.q(&[q, "--json"]);
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    let raw = stdout(&o);
    assert!(
        raw.is_ascii() || !raw.contains('\u{202e}'),
        "raw U+202E in JSON: {raw:?}"
    );
    assert!(
        !raw.contains('\u{1b}') && !raw.contains('\u{9b}'),
        "raw control: {raw:?}"
    );
    assert!(
        raw.to_lowercase().contains("\\u202e"),
        "U+202E not \\u-escaped: {raw}"
    );
    assert!(
        raw.to_lowercase().contains("\\u001b"),
        "ESC not \\u-escaped: {raw}"
    );
    let j: Json = serde_json::from_str(raw.trim()).unwrap();
    assert_eq!(j["rows"][0][0]["value"].as_str().unwrap(), CTRL_NAME);
}

#[test]
fn bell_in_string_is_json_escaped_and_round_trips() {
    let name = "x\u{7}y\u{202e}z";
    let y = "schema:\n  node_types: [CHARACTER]\n  attributes:\n    - { name: name, type: string }\nentities:\n  CHARACTER: [b]\ntransactions:\n  - tick: 1\n    ops:\n      - assert: [b, name, \"x\\u0007y\\u202Ez\"]\n";
    let w = load_text(y);
    let o = w.q(&["[:find ?n :where [?e :name ?n]]", "--json"]);
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    let raw = stdout(&o);
    assert!(
        !raw.contains('\u{7}') && !raw.contains('\u{202e}'),
        "{raw:?}"
    );
    assert!(raw.to_lowercase().contains("\\u0007"), "{raw}");
    let j: Json = serde_json::from_str(raw.trim()).unwrap();
    assert_eq!(j["rows"][0][0]["value"].as_str().unwrap(), name);
    // text mode never prints raw controls either
    let t = w.q(&["[:find ?n :where [?e :name ?n]]"]);
    no_raw_controls(&stdout(&t));
}

fn query_labels(w: &W, who: &str, t: i64, c: u64) -> Vec<String> {
    let o = w.q(&[
        ALLIES_AT,
        "--param",
        &format!("who={who}"),
        "--param",
        &format!("t={t}"),
        "--param",
        &format!("c={c}"),
        "--json",
    ]);
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    let j: Json = serde_json::from_str(stdout(&o).trim()).unwrap();
    let mut v: Vec<String> = j["rows"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r[0]["value"]["label"].as_str().unwrap().to_string())
        .collect();
    v.sort();
    v
}

fn neighbor_labels(w: &W, who: &str, t: i64, c: u64) -> Vec<String> {
    let o = w.run(
        "neighbors",
        &[
            who,
            "ally_of",
            "--both",
            "--at",
            &t.to_string(),
            "--canon",
            &c.to_string(),
            "--json",
        ],
    );
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    let j: Json = serde_json::from_str(stdout(&o).trim()).unwrap();
    let mut v: Vec<String> = j["neighbors"]
        .as_array()
        .unwrap()
        .iter()
        .map(|n| n["entity"]["label"].as_str().unwrap().to_string())
        .collect();
    v.sort();
    v
}

#[test]
fn query_agrees_with_neighbors_both_across_ticks_and_canons() {
    let w = varrow();
    for (t, c) in [
        (-210, 12),
        (-200, 12),
        (700, 12),
        (-210, 11),
        (-200, 11),
        (700, 11),
    ] {
        let a = query_labels(&w, "edwin", t, c);
        let b = neighbor_labels(&w, "edwin", t, c);
        assert_eq!(a, b, "tick {t} canon {c}");
    }
    // the retcon (tx#12) must matter somewhere, or the canon check proves nothing
    let differs = [-210, -200, 700]
        .iter()
        .any(|&t| neighbor_labels(&w, "edwin", t, 11) != neighbor_labels(&w, "edwin", t, 12));
    assert!(
        differs,
        "fixture's tx#12 changes no ally set at these ticks"
    );
}

#[test]
fn query_and_explain_leave_world_file_untouched() {
    let w = varrow();
    let before = std::fs::read(&w.path).unwrap();
    let names0 = dir_names(&w.dir);
    let m0 = std::fs::metadata(&w.path).unwrap().modified().unwrap();
    let _ = w.q(&[ALLIES, "--param", "who=edwin"]);
    let _ = w.q(&["[:find ?a :where [?a :name ?x] [?b :name ?y] [?c :name ?z] [?d :name ?w]]"]);
    let _ = w.run("explain", &[ALLIES, "--param", "who=edwin"]);
    assert_eq!(std::fs::read(&w.path).unwrap(), before);
    assert_eq!(std::fs::metadata(&w.path).unwrap().modified().unwrap(), m0);
    assert_eq!(dir_names(&w.dir), names0);
}
