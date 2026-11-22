//! Slice 7, item 8 (RED): `kala doctor <WORLD> --checks FILE [--canon C] [--json]`.
//!
//! Contract (tech lead): FILE is an EDN vector of `[:name [:find ...]]` checks
//! (see `kala-datalog/tests/checks_parse.rs`), read like `query -f`. After the
//! world's own doctor layers, every check is prepared first (any error, or a
//! check with `:in` params: exit 1, `kala: error: check :name: <located
//! message>`, nothing printed); then each runs with the default limits at the
//! latest canon, or at `--canon` (overriding the query's own `:canon`). A check
//! with >= 1 row is a hint (kind `story_check`); hints never change the exit
//! code. Text: after warnings, `hints (N):`, per check
//! `  story_check: :name: K rows` (`1 row`), up to 5 rows indented 4 with the
//! values as `kala query` prints them, then `    ... (K-5 more)`. JSON gains
//! `"hints": [{"kind","check","rows_total","columns","rows"}]` (<= 100 rows,
//! values as `kala query --json`; present and `[]` without `--checks`).
//! `clean` only with no errors, warnings or hints.
//!
//! Oracles: the rows of a check are what `kala query` prints for the same
//! text, so expected rows are read from `kala query` and the hand-derived
//! counts (in comments) are asserted next to them.
//!
//! varrow.yaml facts used (latest tx#12, latest tick 700; the world is doctor-
//! clean at the latest canon; at canon 8 greg is in varrow and ashford at 601):
//! - Greg in two places at 601: 0 rows at tx#12 (he left Varrow at 250);
//!   2 rows at canon 8: (varrow ashford) and (ashford varrow).
//! - Eldoria's ruler: edwin since -180, never retracted: 1 row `edwin`.
//! - Villagers: v01..v24 carry label "Villager" from -400 on: 24 rows at
//!   tick 0; the cross product of two such patterns is 24 * 24 = 576 rows.

mod common;
#[path = "common/damage.rs"]
mod damage;

use common::*;
use damage::{file_with, spans};
use serde_json::{Value as Json, json};
use std::path::PathBuf;

const GREG2: &str =
    "[:find ?a ?b :at 601 :where [:greg :located_in ?a] [:greg :located_in ?b] [(!= ?a ?b)]]";
const RULER: &str = "[:find ?r :where [:eldoria :ruled_by ?r]]";
const VILLAGERS: &str = "[:find ?c ?n :at 0 :where [?c :label \"Villager\"] [?c :name ?n]]";
const PAIRS: &str = "[:find ?a ?b :at 0 :where [?a :label \"Villager\"] [?b :label \"Villager\"]]";
const EMPTY: &str = "[:find ?c :at 0 :where [?c :label \"NoSuchLabel\"]]";

fn file(fx: &Fx, name: &str, text: &str) -> PathBuf {
    let p = fx.dir.path().join(name);
    std::fs::write(&p, text).unwrap();
    p
}

fn checks(items: &[(&str, &str)]) -> String {
    let body: Vec<String> = items.iter().map(|(n, q)| format!(" [:{n} {q}]")).collect();
    format!("[\n{}\n]\n", body.join("\n"))
}

fn doctor(fx: &Fx, f: &std::path::Path, extra: &[&str]) -> std::process::Output {
    let mut a = vec!["--checks", f.to_str().unwrap()];
    a.extend_from_slice(extra);
    fx.run("doctor", &a)
}

/// The row lines `kala query` prints for `q` (after its two header lines).
fn query_row_lines(fx: &Fx, q: &str) -> Vec<String> {
    let out = fx.text("query", &[q]);
    out.lines()
        .skip(2)
        .filter(|l| *l != "  (no rows)")
        .map(str::to_string)
        .collect()
}

fn query_json(fx: &Fx, q: &str) -> Json {
    fx.json("query", &[q])
}

/// The hint block text for a check whose rows are `rows` (query row lines).
fn block(name: &str, rows: &[String]) -> String {
    let k = rows.len();
    let mut s = format!(
        "  story_check: :{name}: {k} {}\n",
        if k == 1 { "row" } else { "rows" }
    );
    for r in rows.iter().take(5) {
        s.push_str(&format!("  {r}\n"));
    }
    if k > 5 {
        s.push_str(&format!("    ... ({} more)\n", k - 5));
    }
    s
}

// ------------------------------------------------------------------ text

#[test]
fn rows_found_zero_rows_and_truncation_in_one_run() {
    let fx = varrow();
    let f = file(
        &fx,
        "c.edn",
        &checks(&[
            ("greg-two", GREG2),
            ("ruler", RULER),
            ("villagers", VILLAGERS),
            ("none", EMPTY),
        ]),
    );
    let o = doctor(&fx, &f, &[]);
    assert_eq!(
        code(&o),
        0,
        "hints never change the exit code: {}",
        stderr(&o)
    );
    let out = stdout(&o);
    // 1 row (singular), 24 rows (truncated to 5 + "19 more"); 0-row checks print nothing
    let villagers = query_row_lines(&fx, VILLAGERS);
    assert_eq!(villagers.len(), 24);
    let ruler = query_row_lines(&fx, RULER);
    assert_eq!(ruler, ["  edwin"]);
    let want = format!(
        "hints (2):\n{}{}",
        block("ruler", &ruler),
        block("villagers", &villagers)
    );
    assert_eq!(out, want);
    assert!(out.contains("  story_check: :ruler: 1 row\n"));
    assert!(out.contains("  story_check: :villagers: 24 rows\n"));
    assert!(out.contains("    ... (19 more)\n"));
    assert!(!out.contains("greg-two") && !out.contains(":none"));
    assert!(!out.lines().any(|l| l == "clean"));
    no_raw_controls(&out);
}

#[test]
fn exactly_five_rows_has_no_more_line_and_six_has_one() {
    let fx = varrow();
    let five =
        "[:find ?c :at 0 :where [?c :label \"Villager\"] [?c :name ?n] [(< ?n \"Villager 06\")]]";
    let six =
        "[:find ?c :at 0 :where [?c :label \"Villager\"] [?c :name ?n] [(< ?n \"Villager 07\")]]";
    let f = file(&fx, "c.edn", &checks(&[("five", five), ("six", six)]));
    let out = stdout(&doctor(&fx, &f, &[]));
    // names "Villager 01".. sort as strings: < "Villager 06" is 5 villagers, < "Villager 07" is 6
    assert_eq!(query_row_lines(&fx, five).len(), 5);
    assert_eq!(query_row_lines(&fx, six).len(), 6);
    assert!(out.contains("  story_check: :five: 5 rows\n"), "{out}");
    assert!(out.contains("  story_check: :six: 6 rows\n"), "{out}");
    assert_eq!(out.matches("... (").count(), 1, "{out}");
    assert!(out.contains("    ... (1 more)\n"), "{out}");
}

#[test]
fn hint_order_follows_the_file_and_each_row_is_indented_four() {
    let fx = varrow();
    let f = file(
        &fx,
        "c.edn",
        &checks(&[("z-last-alpha", RULER), ("a-first-alpha", VILLAGERS)]),
    );
    let out = stdout(&doctor(&fx, &f, &[]));
    let z = out.find(":z-last-alpha").unwrap();
    let a = out.find(":a-first-alpha").unwrap();
    assert!(z < a, "file order, not alphabetical: {out}");
    assert!(out.contains("\n    edwin\n"), "{out}");
}

#[test]
fn all_checks_empty_prints_clean_and_exits_0() {
    let fx = varrow();
    let f = file(
        &fx,
        "c.edn",
        &checks(&[("greg-two", GREG2), ("none", EMPTY)]),
    );
    let o = doctor(&fx, &f, &[]);
    assert_eq!(code(&o), 0);
    assert_eq!(stdout(&o), "clean\n");
}

#[test]
fn an_empty_list_of_checks_is_clean() {
    let fx = varrow();
    let f = file(&fx, "c.edn", "[]\n");
    let o = doctor(&fx, &f, &[]);
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    assert_eq!(stdout(&o), "clean\n");
}

#[test]
fn without_checks_the_output_is_unchanged() {
    let fx = varrow();
    assert_eq!(fx.text("doctor", &[]), "clean\n");
}

// ------------------------------------------------------------------ canon

#[test]
fn canon_runs_the_checks_at_that_canon_and_after_the_warnings() {
    let fx = varrow();
    let f = file(&fx, "c.edn", &checks(&[("greg-two", GREG2)]));
    // the world's own doctor at canon 8 warns about greg's overlap (draft)
    let plain = fx.text("doctor", &["--canon", "8"]);
    assert!(plain.contains("warnings (1):"), "{plain}");
    let o = doctor(&fx, &f, &["--canon", "8"]);
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    let out = stdout(&o);
    assert!(
        out.starts_with(&plain),
        "doctor sections come first:\n{out}"
    );
    // hand-derived: 2 rows at canon 8 (varrow ashford / ashford varrow)
    let rows = vec![
        "  varrow  ashford".to_string(),
        "  ashford  varrow".to_string(),
    ];
    assert_eq!(
        &out[plain.len()..],
        format!("hints (1):\n{}", block("greg-two", &rows))
    );
    assert!(out.find("warnings (").unwrap() < out.find("hints (").unwrap());
}

#[test]
fn canon_overrides_the_query_own_canon_both_ways() {
    let fx = varrow();
    // the query pins canon 12 (no overlap), --canon 8 wins: 2 rows
    let pinned_latest = "[:find ?a ?b :at 601 :canon 12 :where [:greg :located_in ?a] [:greg :located_in ?b] [(!= ?a ?b)]]";
    let f = file(&fx, "c.edn", &checks(&[("g", pinned_latest)]));
    let out = stdout(&doctor(&fx, &f, &["--canon", "8"]));
    assert!(out.contains("  story_check: :g: 2 rows\n"), "{out}");
    // the query pins canon 8 (overlap), --canon 12 wins: 0 rows, clean
    let pinned_draft = "[:find ?a ?b :at 601 :canon 8 :where [:greg :located_in ?a] [:greg :located_in ?b] [(!= ?a ?b)]]";
    let f = file(&fx, "d.edn", &checks(&[("g", pinned_draft)]));
    let o = doctor(&fx, &f, &["--canon", "12"]);
    assert_eq!(stdout(&o), "clean\n", "{}", stderr(&o));
    // `tx#N` is accepted as for doctor today
    let o = doctor(&fx, &f, &["--canon", "tx#12"]);
    assert_eq!(stdout(&o), "clean\n");
}

#[test]
fn the_latest_canon_is_the_default() {
    let fx = varrow();
    let f = file(&fx, "c.edn", &checks(&[("greg-two", GREG2)]));
    assert_eq!(stdout(&doctor(&fx, &f, &[])), "clean\n");
    assert_eq!(stdout(&doctor(&fx, &f, &["--canon", "12"])), "clean\n");
}

// ------------------------------------------------------------------ json

#[test]
fn json_hints_match_kala_query_json() {
    let fx = varrow();
    let f = file(
        &fx,
        "c.edn",
        &checks(&[("ruler", RULER), ("none", EMPTY), ("villagers", VILLAGERS)]),
    );
    let o = doctor(&fx, &f, &["--json"]);
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    let j = parse_one_json(&stdout(&o));
    assert_eq!(j["command"], json!("doctor"));
    assert_eq!(j["clean"], json!(false));
    assert_eq!(j["errors"], json!([]));
    assert_eq!(j["warnings"], json!([]));
    let hints = j["hints"].as_array().expect("hints array");
    assert_eq!(hints.len(), 2, "{j}");
    let (ruler, vil) = (query_json(&fx, RULER), query_json(&fx, VILLAGERS));
    assert_eq!(hints[0]["kind"], json!("story_check"));
    assert_eq!(hints[0]["check"], json!("ruler"));
    assert_eq!(hints[0]["rows_total"], json!(1));
    assert_eq!(hints[0]["columns"], ruler["columns"]);
    assert_eq!(hints[0]["rows"], ruler["rows"]);
    assert_eq!(hints[0]["columns"], json!(["?r"]));
    assert_eq!(hints[1]["check"], json!("villagers"));
    assert_eq!(hints[1]["rows_total"], json!(24));
    assert_eq!(hints[1]["columns"], vil["columns"]);
    assert_eq!(hints[1]["rows"], vil["rows"]);
    assert_eq!(hints[1]["rows"].as_array().unwrap().len(), 24);
    // exactly these keys per hint
    let mut keys: Vec<&String> = hints[0].as_object().unwrap().keys().collect();
    keys.sort();
    assert_eq!(keys, ["check", "columns", "kind", "rows", "rows_total"]);
}

#[test]
fn json_rows_are_capped_at_100_but_rows_total_is_exact() {
    let fx = varrow();
    let f = file(&fx, "c.edn", &checks(&[("pairs", PAIRS)]));
    let j = parse_one_json(&stdout(&doctor(&fx, &f, &["--json"])));
    let h = &j["hints"][0];
    assert_eq!(h["rows_total"], json!(576));
    let rows = h["rows"].as_array().unwrap();
    assert_eq!(rows.len(), 100);
    let all = query_json(&fx, PAIRS);
    assert_eq!(all["rows"].as_array().unwrap().len(), 576);
    assert_eq!(rows[..], all["rows"].as_array().unwrap()[..100]);
    // and the text output truncates at 5
    let out = stdout(&doctor(&fx, &f, &[]));
    assert!(out.contains("  story_check: :pairs: 576 rows\n"), "{out}");
    assert!(out.contains("    ... (571 more)\n"), "{out}");
}

#[test]
fn json_without_checks_has_an_empty_hints_array_and_the_rest_unchanged() {
    let fx = varrow();
    let j = fx.json("doctor", &[]);
    assert_eq!(j["hints"], json!([]));
    assert_eq!(j["clean"], json!(true));
    assert_eq!(j["errors"], json!([]));
    assert_eq!(j["warnings"], json!([]));
    assert_eq!(j["command"], json!("doctor"));
    assert!(j.get("canon").is_some() && j.get("since").is_some());
}

#[test]
fn json_all_checks_empty_is_clean_with_empty_hints() {
    let fx = varrow();
    let f = file(&fx, "c.edn", &checks(&[("none", EMPTY)]));
    let j = parse_one_json(&stdout(&doctor(&fx, &f, &["--json"])));
    assert_eq!(j["clean"], json!(true));
    assert_eq!(j["hints"], json!([]));
}

#[test]
fn json_with_a_warning_and_a_hint_is_not_clean_and_keeps_both() {
    let fx = varrow();
    let f = file(&fx, "c.edn", &checks(&[("greg-two", GREG2)]));
    let j = parse_one_json(&stdout(&doctor(&fx, &f, &["--canon", "8", "--json"])));
    assert_eq!(j["canon"], json!(8));
    assert_eq!(j["clean"], json!(false));
    assert_eq!(j["warnings"].as_array().unwrap().len(), 1);
    assert_eq!(j["hints"].as_array().unwrap().len(), 1);
    assert_eq!(j["hints"][0]["rows_total"], json!(2));
}

#[test]
fn json_aggregate_columns_use_the_aggregate_text() {
    let fx = varrow();
    let q = "[:find (count ?c) :at 0 :where [?c :label \"Villager\"]]";
    let f = file(&fx, "c.edn", &checks(&[("count", q)]));
    let j = parse_one_json(&stdout(&doctor(&fx, &f, &["--json"])));
    assert_eq!(j["hints"][0]["columns"], json!(["(count ?c)"]));
    assert_eq!(j["hints"][0]["rows"], query_json(&fx, q)["rows"]);
    assert_eq!(j["hints"][0]["rows"][0][0]["value"], json!(24));
}

// ------------------------------------------------------------------ errors

#[test]
fn a_broken_check_exits_1_names_the_check_and_prints_nothing_else() {
    let fx = varrow();
    for (what, bad, needle) in [
        (
            "unknown attribute",
            "[:find ?x :where [?x :zzz 1]]",
            "line 3, column 32",
        ),
        (
            "syntax",
            "[:find ?x :where [?x :name]]",
            "line 3, column 28",
        ),
        (
            "unbound",
            "[:find ?y :where [?x :name \"a\"]]",
            "line 3, column 18",
        ),
    ] {
        // a good check with hints comes first: nothing of it may be printed
        let f = file(&fx, "c.edn", &checks(&[("good", RULER), ("broken", bad)]));
        let err = fx.fails(1, "doctor", &["--checks", f.to_str().unwrap()]);
        assert!(
            err.starts_with("kala: error: check :broken: "),
            "{what}: {err}"
        );
        assert!(err.contains(needle), "{what}: {err}");
        assert!(
            err.contains("zzz") || what != "unknown attribute",
            "{what}: {err}"
        );
        assert!(!err.contains(":good"), "{what}: {err}");
    }
}

#[test]
fn a_broken_check_with_json_also_prints_nothing_on_stdout() {
    let fx = varrow();
    let f = file(
        &fx,
        "c.edn",
        &checks(&[("broken", "[:find ?x :where [?x :zzz 1]]")]),
    );
    let o = doctor(&fx, &f, &["--json"]);
    assert_eq!(code(&o), 1);
    assert_eq!(stdout(&o), "");
    assert!(stderr(&o).starts_with("kala: error: check :broken: "));
}

#[test]
fn the_first_broken_check_in_file_order_is_reported() {
    let fx = varrow();
    let f = file(
        &fx,
        "c.edn",
        &checks(&[
            ("one", "[:find ?x :where [?x :zzz 1]]"),
            ("two", "[:find ?x :where [?x :yyy 1]]"),
        ]),
    );
    let err = fx.fails(1, "doctor", &["--checks", f.to_str().unwrap()]);
    assert!(err.starts_with("kala: error: check :one: "), "{err}");
}

#[test]
fn a_check_with_in_parameters_is_an_error() {
    let fx = varrow();
    let q = "[:find ?a :in $ ?who :where [?who :located_in ?a]]";
    let f = file(&fx, "c.edn", &checks(&[("good", RULER), ("param", q)]));
    let err = fx.fails(1, "doctor", &["--checks", f.to_str().unwrap()]);
    assert!(err.starts_with("kala: error: check :param: "), "{err}");
}

#[test]
fn a_malformed_checks_file_exits_1_with_a_located_error() {
    let fx = varrow();
    let cases: Vec<(&str, String, Option<&str>)> = vec![
        ("empty file", String::new(), None),
        ("comment only", "; nothing\n".into(), None),
        ("missing name", format!("[\n [{RULER}]\n]"), Some("line 2")),
        (
            "name not a keyword",
            format!("[\n [\"a\" {RULER}]\n]"),
            Some("line 2"),
        ),
        (
            "extra element",
            format!("[\n [:a {RULER} 1]\n]"),
            Some("line 2"),
        ),
        ("trailing input", "[] []".to_string(), Some("line 1")),
        ("not a vector", format!("[:a {RULER}]"), None),
        (
            "duplicate",
            format!("[\n [:a {RULER}]\n [:a {RULER}]\n]"),
            Some("line 3"),
        ),
    ];
    for (what, text, line) in cases {
        let f = file(&fx, "c.edn", &text);
        let err = fx.fails(1, "doctor", &["--checks", f.to_str().unwrap()]);
        if let Some(l) = line {
            assert!(err.contains(l), "{what}: {err}");
        }
    }
}

#[test]
fn a_missing_checks_file_exits_1() {
    let fx = varrow();
    let p = fx.dir.path().join("nope.edn");
    fx.fails(1, "doctor", &["--checks", p.to_str().unwrap()]);
}

#[test]
fn a_directory_as_the_checks_file_exits_1() {
    let fx = varrow();
    fx.fails(1, "doctor", &["--checks", fx.dir.path().to_str().unwrap()]);
}

#[test]
fn a_non_utf8_checks_file_exits_1() {
    let fx = varrow();
    let p = fx.dir.path().join("bad.edn");
    std::fs::write(&p, b"[[:a [:find ?x :where [?x :name \"\xff\"]]]]").unwrap();
    fx.fails(1, "doctor", &["--checks", p.to_str().unwrap()]);
}

#[test]
fn the_checks_file_size_limit_boundary() {
    let fx = varrow();
    let base = checks(&[("r", RULER)]);
    let at = format!("{base}{}", " ".repeat((1 << 20) - base.len()));
    assert_eq!(at.len(), 1 << 20);
    let f = file(&fx, "ok.edn", &at);
    let o = doctor(&fx, &f, &[]);
    assert_eq!(code(&o), 0, "at the limit: {}", stderr(&o));
    assert!(stdout(&o).contains(":r: 1 row"));
    let f = file(&fx, "big.edn", &format!("{at} "));
    fx.fails(1, "doctor", &["--checks", f.to_str().unwrap()]);
}

#[test]
fn the_check_count_limit_boundary() {
    let fx = varrow();
    let n = |k: usize| {
        let items: Vec<(String, &str)> = (0..k).map(|i| (format!("c{i}"), EMPTY)).collect();
        let refs: Vec<(&str, &str)> = items.iter().map(|(a, b)| (a.as_str(), *b)).collect();
        checks(&refs)
    };
    let f = file(&fx, "ok.edn", &n(256));
    let o = doctor(&fx, &f, &[]);
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    assert_eq!(stdout(&o), "clean\n");
    let f = file(&fx, "many.edn", &n(257));
    fx.fails(1, "doctor", &["--checks", f.to_str().unwrap()]);
}

#[test]
fn a_runtime_limit_error_exits_1_and_names_the_check() {
    // 24^4 = 331776 rows of an unconstrained product: over the default row limit
    let fx = varrow();
    let q = "[:find ?a ?b ?c ?d :at 0 :where [?a :label \"Villager\"] [?b :label \"Villager\"] \
             [?c :label \"Villager\"] [?d :label \"Villager\"]]";
    let f = file(&fx, "c.edn", &checks(&[("huge", q)]));
    // `kala query` stops this query at 100000 rows ("narrow the query")
    let o = doctor(&fx, &f, &[]);
    assert_eq!(code(&o), 1, "{}", stdout(&o));
    assert!(
        stderr(&o).starts_with("kala: error: check :huge: "),
        "{}",
        stderr(&o)
    );
    assert!(stderr(&o).contains("100000"), "{}", stderr(&o));
    assert_eq!(stdout(&o), "");
}

#[test]
fn the_checks_option_needs_a_value() {
    let fx = varrow();
    let o = fx.run("doctor", &["--checks"]);
    assert_eq!(code(&o), 2);
}

// ------------------------------------------------------------------ read-only, escaping

#[test]
fn checks_leave_the_world_file_and_its_directory_untouched() {
    let fx = varrow();
    let f = file(&fx, "c.edn", &checks(&[("ruler", RULER)]));
    let before = std::fs::read(&fx.path).unwrap();
    let names = dir_names(fx.dir.path());
    let o = doctor(&fx, &f, &[]);
    assert_eq!(code(&o), 0);
    assert_eq!(std::fs::read(&fx.path).unwrap(), before);
    assert_eq!(dir_names(fx.dir.path()), names);
}

#[test]
fn row_values_are_escaped_like_kala_query() {
    let fx = build(&ctrl_fixture());
    let q = "[:find ?n :where [:ctrl :name ?n]]";
    let f = file(&fx, "c.edn", &checks(&[("ctrl", q)]));
    let o = doctor(&fx, &f, &[]);
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    let out = stdout(&o);
    no_raw_controls(&out);
    assert!(
        !out.contains('\u{202e}'),
        "bidi override must be escaped: {out:?}"
    );
    let rows = query_row_lines(&fx, q);
    assert_eq!(rows.len(), 1);
    assert_eq!(out, format!("hints (1):\n{}", block("ctrl", &rows)));
    // JSON carries the exact string
    let j = parse_one_json(&stdout(&doctor(&fx, &f, &["--json"])));
    assert_eq!(j["hints"][0]["rows"][0][0]["value"], json!(CTRL_NAME));
}

// ------------------------------------------------------------------ story-check example

/// CLAUDE.md's "dead characters with active states", on a small world.
///
/// edwin, greg, mara are alive from tick 0. greg holds the sword and mara the
/// crown (from 0); edwin holds nothing. Greg dies at 10 and Edwin at 20 (an
/// explicit retract of `alive true`), but Greg's `holds` is never retracted
/// ("states end only when retracted"). Latest tick: 20.
const DEAD_WORLD: &str = r#"
schema:
  node_types: [CHARACTER, ITEM]
  attributes:
    - { name: alive, type: bool }
    - { name: holds, type: ref, cardinality: many }
entities:
  CHARACTER: [edwin, greg, mara]
  ITEM: [sword, crown]
transactions:
  - tick: 0
    ops:
      - assert: [edwin, alive, true]
      - assert: [greg, alive, true]
      - assert: [mara, alive, true]
      - assert: [greg, holds, sword]
      - assert: [mara, holds, crown]
  - tick: 10
    ops:
      - retract: [greg, alive, true]
  - tick: 20
    ops:
      - retract: [edwin, alive, true]
"#;

const DEAD_HOLDING: &str = "[:find ?c ?i :where [?c :holds ?i] (not [?c :alive true])]";
const DEAD_HOLDING_EARLY: &str = "[:find ?c ?i :at 5 :where [?c :holds ?i] (not [?c :alive true])]";
const DEATHS: &str = "[:find ?c ?t :where [?c :alive true ?t :retract]]";

#[test]
fn dead_characters_with_active_states_on_a_small_world() {
    let fx = build(DEAD_WORLD);
    assert_eq!(fx.text("doctor", &[]), "clean\n");
    let f = file(
        &fx,
        "c.edn",
        &checks(&[
            ("dead-holding", DEAD_HOLDING),
            ("dead-holding-early", DEAD_HOLDING_EARLY),
            ("deaths", DEATHS),
        ]),
    );
    let o = doctor(&fx, &f, &[]);
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    // at the latest tick (20): only greg is dead and holding (the sword);
    // at tick 5 nobody is dead; two deaths: edwin @20, greg @10
    let holding = vec!["  greg  sword".to_string()];
    assert_eq!(query_row_lines(&fx, DEAD_HOLDING), holding);
    assert!(query_row_lines(&fx, DEAD_HOLDING_EARLY).is_empty());
    let mut deaths = query_row_lines(&fx, DEATHS);
    deaths.sort();
    assert_eq!(deaths, ["  edwin  20", "  greg  10"]);
    let out = stdout(&o);
    assert!(out.starts_with("hints (2):\n"), "{out}");
    assert!(out.contains(&block("dead-holding", &holding)), "{out}");
    assert!(!out.contains("dead-holding-early"), "{out}");
    assert!(out.contains("  story_check: :deaths: 2 rows\n"), "{out}");
    assert!(out.contains("    greg  sword\n"), "{out}");
    assert!(
        out.contains("    greg  10\n") && out.contains("    edwin  20\n"),
        "{out}"
    );
}

#[test]
fn the_story_check_follows_a_retraction() {
    // mara is alive; once greg's `holds` is retracted the check is empty
    let yaml = DEAD_WORLD.replace(
        "      - retract: [edwin, alive, true]\n",
        "      - retract: [edwin, alive, true]\n      - retract: [greg, holds, sword]\n",
    );
    assert_ne!(yaml, DEAD_WORLD);
    let fx = build(&yaml);
    let f = file(&fx, "c.edn", &checks(&[("dead-holding", DEAD_HOLDING)]));
    let o = doctor(&fx, &f, &[]);
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    assert_eq!(stdout(&o), "clean\n");
}

#[test]
fn story_checks_use_the_world_schema_at_the_latest_canon() {
    // an attribute the world doesn't have is a compile error naming the check
    let fx = build(DEAD_WORLD);
    let f = file(
        &fx,
        "c.edn",
        &checks(&[("bad", "[:find ?c :where [?c :located_in ?p]]")]),
    );
    let err = fx.fails(1, "doctor", &["--checks", f.to_str().unwrap()]);
    assert!(err.starts_with("kala: error: check :bad: "), "{err}");
}

// ------------------------------------------------------------------ slice 7 item 8 confirm

/// The varrow world with its last transaction torn mid-way.
fn torn_varrow() -> (tempfile::TempDir, PathBuf) {
    let fx = varrow();
    let mut bytes = std::fs::read(&fx.path).unwrap();
    let last = *spans(&bytes).last().unwrap();
    bytes.truncate(last.start + 10);
    file_with(&bytes)
}

#[test]
fn store_errors_skip_the_checks_text() {
    let (dir, p) = torn_varrow();
    let f = dir.path().join("c.edn");
    std::fs::write(&f, checks(&[("ruler", RULER)])).unwrap();
    let before = std::fs::read(&p).unwrap();
    let o = kala([
        "doctor".as_ref(),
        p.as_os_str(),
        "--checks".as_ref(),
        f.as_os_str(),
    ]);
    assert_eq!(code(&o), 1, "{}", stderr(&o));
    let out = stdout(&o);
    assert!(out.contains("errors (1):\n"), "{out}");
    assert!(
        out.ends_with("hints: skipped (the store has errors)\n"),
        "{out}"
    );
    assert!(!out.contains("story_check"), "{out}");
    assert_eq!(std::fs::read(&p).unwrap(), before, "read-only");
}

#[test]
fn store_errors_skip_the_checks_json() {
    let (dir, p) = torn_varrow();
    let f = dir.path().join("c.edn");
    std::fs::write(&f, checks(&[("ruler", RULER)])).unwrap();
    let o = kala([
        "doctor".as_ref(),
        p.as_os_str(),
        "--checks".as_ref(),
        f.as_os_str(),
        "--json".as_ref(),
    ]);
    assert_eq!(code(&o), 1, "{}", stderr(&o));
    let j = parse_one_json(&stdout(&o));
    assert_eq!(j["checks_skipped"], json!(true));
    assert_eq!(j["hints"], json!([]));
    assert_eq!(j["clean"], json!(false));
    assert_eq!(j["errors"].as_array().unwrap().len(), 1);
}

#[test]
fn checks_skipped_is_false_without_errors_or_without_checks() {
    let fx = varrow();
    assert_eq!(fx.json("doctor", &[])["checks_skipped"], json!(false));
    let f = file(&fx, "c.edn", &checks(&[("ruler", RULER)]));
    let j = parse_one_json(&stdout(&doctor(&fx, &f, &["--json"])));
    assert_eq!(j["checks_skipped"], json!(false));
    assert_eq!(j["hints"].as_array().unwrap().len(), 1);
    // a damaged store without --checks: nothing was skipped
    let (_d, p) = torn_varrow();
    let o = kala(["doctor".as_ref(), p.as_os_str(), "--json".as_ref()]);
    assert_eq!(code(&o), 1);
    let j = parse_one_json(&stdout(&o));
    assert_eq!(j["checks_skipped"], json!(false));
    assert!(!stdout(&o).contains("hints: skipped"));
}

#[test]
fn a_bad_checks_file_exits_1_even_on_a_damaged_store() {
    let (dir, p) = torn_varrow();
    for (what, text) in [
        ("duplicate", format!("[[:a {RULER}] [:a {RULER}]]")),
        ("not a vector", format!("[:a {RULER}]")),
        ("empty", String::new()),
    ] {
        let f = dir.path().join("bad.edn");
        std::fs::write(&f, &text).unwrap();
        let o = kala([
            "doctor".as_ref(),
            p.as_os_str(),
            "--checks".as_ref(),
            f.as_os_str(),
        ]);
        assert_eq!(code(&o), 1, "{what}: {}", stderr(&o));
        assert!(stderr(&o).starts_with("kala: error: "), "{what}");
        assert_eq!(stdout(&o), "", "{what}: the store errors are not printed");
    }
    // a missing file likewise
    let o = kala([
        "doctor".as_ref(),
        p.as_os_str(),
        "--checks".as_ref(),
        dir.path().join("nope.edn").as_os_str(),
    ]);
    assert_eq!(code(&o), 1);
    assert_eq!(stdout(&o), "");
}

// NEW CONTRACT: a compile error is located in the checks file.

#[test]
fn a_compile_error_is_located_in_the_checks_file_single_line() {
    let fx = varrow();
    let l3 = " [:broken [:find ?x :where [?x :zzz 1]]]";
    let text = format!("[\n [:good {RULER}]\n{l3}\n]\n");
    let f = file(&fx, "c.edn", &text);
    let err = fx.fails(1, "doctor", &["--checks", f.to_str().unwrap()]);
    let col = l3.find(":zzz").unwrap() + 1; // 1-based, in chars (ASCII)
    assert!(
        err.starts_with(&format!(
            "kala: error: check :broken: line 3, column {col}: "
        )),
        "want line 3, column {col}: {err}"
    );
    assert!(err.contains("zzz"), "{err}");
}

#[test]
fn a_compile_error_is_located_in_the_checks_file_multi_line() {
    let fx = varrow();
    // the unknown attribute sits on file line 5, after four leading spaces
    let text = "; story checks\n[\n [:first [:find ?r :where [:eldoria :ruled_by ?r]]]\n [:second [:find ?x\n    :where [?x :zzz 1]]]\n]\n";
    let f = file(&fx, "c.edn", text);
    let err = fx.fails(1, "doctor", &["--checks", f.to_str().unwrap()]);
    let l5 = text.lines().nth(4).unwrap();
    let col = l5.find(":zzz").unwrap() + 1;
    assert!(
        err.starts_with(&format!(
            "kala: error: check :second: line 5, column {col}: "
        )),
        "want line 5, column {col}: {err}"
    );
}

#[test]
fn a_syntax_error_is_located_in_the_checks_file_with_non_ascii_before_it() {
    let fx = varrow();
    // columns count chars, not bytes
    let l3 = " [:nm [:find ?x :where [?x :name \"Åberg ☃\"] [?x :name]]]";
    let f = file(&fx, "c.edn", &format!("[\n ; é\n{l3}\n]\n"));
    let err = fx.fails(1, "doctor", &["--checks", f.to_str().unwrap()]);
    assert!(
        err.starts_with("kala: error: check :nm: line 3, column "),
        "{err}"
    );
    let col: usize = err["kala: error: check :nm: line 3, column ".len()..]
        .split(':')
        .next()
        .unwrap()
        .parse()
        .unwrap();
    // the error is on the 2-element pattern `[?x :name]` (columns count chars)
    let pat = l3[..l3.find("[?x :name]").unwrap()].chars().count() + 1;
    assert!(
        (pat..pat + "[?x :name]".len()).contains(&col),
        "col {col} not inside the pattern starting at {pat}: {err}"
    );
}

#[test]
fn an_error_at_the_first_byte_of_a_checks_query_points_at_its_bracket() {
    let fx = varrow();
    // an empty query vector is rejected as a whole, at its own `[`
    let l3 = " [:first []]";
    let f = file(&fx, "c.edn", &format!("[\n [:ok {RULER}]\n{l3}\n]\n"));
    let err = fx.fails(1, "doctor", &["--checks", f.to_str().unwrap()]);
    let col = l3.find("[]").unwrap() + 1;
    assert!(
        err.starts_with(&format!(
            "kala: error: check :first: line 3, column {col}: "
        )),
        "want line 3, column {col}: {err}"
    );
}

#[test]
fn columns_count_chars_after_a_non_ascii_comment_on_the_same_line() {
    let fx = varrow();
    let l2 = " [:nm ; é☃Å\n";
    let l3 = "   [:find ?x :where [?x :zzz 1]]]";
    let f = file(&fx, "c.edn", &format!("[\n{l2}{l3}\n]\n"));
    let err = fx.fails(1, "doctor", &["--checks", f.to_str().unwrap()]);
    let col = l3.find(":zzz").unwrap() + 1;
    assert!(
        err.starts_with(&format!("kala: error: check :nm: line 3, column {col}: ")),
        "want line 3, column {col}: {err}"
    );
}

#[test]
fn columns_count_chars_after_a_non_ascii_comment_on_the_line_before() {
    let fx = varrow();
    let l3 = " [:nm [:find ?x :where [?x :zzz \"é☃\"]]]";
    let f = file(&fx, "c.edn", &format!("[\n ; Åé☃\n{l3}\n]\n"));
    let err = fx.fails(1, "doctor", &["--checks", f.to_str().unwrap()]);
    let col = l3[..l3.find(":zzz").unwrap()].chars().count() + 1;
    assert!(
        err.starts_with(&format!("kala: error: check :nm: line 3, column {col}: ")),
        "want line 3, column {col}: {err}"
    );
}

// edge cases

#[test]
fn canon_above_the_latest_is_clamped_to_the_latest() {
    let fx = varrow();
    let f = file(
        &fx,
        "c.edn",
        &checks(&[("greg-two", GREG2), ("ruler", RULER)]),
    );
    let at_latest = stdout(&doctor(&fx, &f, &[]));
    assert!(at_latest.contains(":ruler: 1 row"), "{at_latest}");
    let o = doctor(&fx, &f, &["--canon", "999"]);
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    assert_eq!(stdout(&o), at_latest, "same as the latest canon");
    // and like `kala query`, whose canon above the latest is the latest
    let q = fx.text("query", &["[:find ?a ?b :at 601 :canon 999 :where [:greg :located_in ?a] [:greg :located_in ?b] [(!= ?a ?b)]]"]);
    assert!(q.contains("no rows"), "{q}");
}

#[test]
fn since_does_not_narrow_the_checks() {
    let fx = varrow();
    let f = file(
        &fx,
        "c.edn",
        &checks(&[("ruler", RULER), ("villagers", VILLAGERS)]),
    );
    let all = stdout(&doctor(&fx, &f, &[]));
    // tx#12 only added Edwin retcons; the ruler fact is from tx#1
    let o = doctor(&fx, &f, &["--since", "12"]);
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    assert_eq!(stdout(&o), all);
    assert!(all.contains(":villagers: 24 rows"));
    let j = parse_one_json(&stdout(&doctor(&fx, &f, &["--since", "12", "--json"])));
    assert_eq!(j["hints"].as_array().unwrap().len(), 2);
    assert_eq!(j["since"], json!(12));
}

#[test]
fn a_deeply_nested_check_is_a_limit_error_exit_1() {
    let fx = varrow();
    let n = 100;
    let q = format!(
        "[:find ?x :where [?x :name _] {}[?x :name _]{}]",
        "(not ".repeat(n),
        ")".repeat(n)
    );
    let f = file(&fx, "c.edn", &checks(&[("deep", &q)]));
    let o = doctor(&fx, &f, &[]);
    assert_eq!(code(&o), 1, "no crash: {}", stderr(&o));
    // nested past MAX_DEPTH: a Limit error (located), whichever layer sees it first
    assert!(
        stderr(&o).starts_with("kala: error: ") && stderr(&o).contains("deeper than"),
        "{}",
        stderr(&o)
    );
    assert_eq!(stdout(&o), "");
    let mut flat = String::new();
    flat.push('[');
    flat.push_str(&"[".repeat(n));
    flat.push_str(&"]".repeat(n));
    flat.push(']');
    let f = file(&fx, "d.edn", &flat);
    let o = doctor(&fx, &f, &[]);
    assert_eq!(code(&o), 1, "deeply nested file, no crash: {}", stderr(&o));
}

#[cfg(unix)]
#[test]
fn a_fifo_checks_file_is_refused_without_hanging() {
    use std::process::Stdio;
    use std::time::{Duration, Instant};
    let fx = varrow();
    let fifo = fx.dir.path().join("pipe");
    if !matches!(std::process::Command::new("mkfifo").arg(&fifo).status(), Ok(s) if s.success()) {
        eprintln!("skipped: mkfifo unavailable");
        return;
    }
    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_kala"))
        .arg("doctor")
        .arg(&fx.path)
        .arg("--checks")
        .arg(&fifo)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(20);
    let st = loop {
        if let Some(st) = child.try_wait().unwrap() {
            break st;
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("hang: a FIFO as the checks file was opened");
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    assert_eq!(st.code(), Some(1));
}
