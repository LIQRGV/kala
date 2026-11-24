//! Slice 6 item 7: `kala query` and `kala explain` (Datalog from the command
//! line), RED tests against the real binary. Contract: "Item 7: CLI".
//!
//! Expected answers come from the kala-core native API (neighbors, Both) on an
//! in-memory twin of `varrow.yaml`, never from hard-coded guesses.
//! Ambiguities are listed in the report, not guessed here.

mod common;

use common::*;
use kala_core::{Direction, EntityId, Tick, TxId, World};
use kala_fixture::load_fixture;
use serde_json::{Value as Json, json};
use std::ffi::OsString;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const RETCON: u64 = 12;
const BEFORE_RETCON: u64 = 11;
const ALLIES: &str = "[:find ?a :in $ ?who :where [?e :kala_handle ?who] [?e :ally_of ?a]]";

struct Fx {
    _dir: tempfile::TempDir,
    dir: PathBuf,
    path: PathBuf,
    twin: World,
}

fn load_yaml(yaml_path: &str) -> Fx {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("w.kala");
    let o = kala([
        OsString::from("load"),
        yaml_path.into(),
        path.clone().into(),
    ]);
    assert_eq!(code(&o), 0, "kala load: {}", stderr(&o));
    let twin = load_fixture(&std::fs::read_to_string(yaml_path).unwrap()).unwrap();
    Fx {
        dir: dir.path().to_path_buf(),
        _dir: dir,
        path,
        twin,
    }
}

fn varrow() -> Fx {
    load_yaml(VARROW)
}

const TYPED: &str = r#"
schema:
  node_types: [CHARACTER]
  attributes:
    - { name: name,  type: string }
    - { name: age,   type: i64 }
    - { name: brave, type: bool }
    - { name: height, type: f64 }
entities:
  CHARACTER: [alice, bob]
transactions:
  - tick: 5
    ops:
      - assert: [alice, name, "Alice"]
      - assert: [alice, age, -5]
      - assert: [alice, brave, true]
"#;

fn typed() -> Fx {
    let dir = tempfile::tempdir().unwrap();
    let yaml = dir.path().join("typed.yaml");
    std::fs::write(&yaml, TYPED).unwrap();
    let path = dir.path().join("w.kala");
    let o = kala([
        OsString::from("load"),
        yaml.clone().into(),
        path.clone().into(),
    ]);
    assert_eq!(code(&o), 0, "kala load: {}", stderr(&o));
    let twin = load_fixture(TYPED).unwrap();
    Fx {
        dir: dir.path().to_path_buf(),
        _dir: dir,
        path,
        twin,
    }
}

impl Fx {
    fn run(&self, cmd: &str, rest: &[&str]) -> std::process::Output {
        let mut a: Vec<OsString> = vec![cmd.into(), self.path.clone().into()];
        a.extend(rest.iter().map(|s| OsString::from(*s)));
        kala(a)
    }

    fn q(&self, rest: &[&str]) -> std::process::Output {
        self.run("query", rest)
    }

    fn json(&self, rest: &[&str]) -> Json {
        let mut r = rest.to_vec();
        r.push("--json");
        let o = self.q(&r);
        assert_eq!(code(&o), 0, "kala query {rest:?}: {}", stderr(&o));
        serde_json::from_str(stdout(&o).trim()).expect("one JSON document")
    }

    fn id(&self, h: &str) -> EntityId {
        self.twin.entity_by_handle(h).unwrap()
    }

    fn label(&self, e: EntityId) -> String {
        self.twin
            .handle_of(e)
            .map(str::to_string)
            .unwrap_or_else(|| self.twin.format_id(e))
    }

    /// Allies of `who` (neighbors, Both) at `t` and canon `c`, as sorted labels.
    fn allies(&self, who: &str, t: i64, c: u64) -> Vec<String> {
        let v = self.twin.at_canon(TxId(c));
        let mut l: Vec<String> = v
            .neighbors(self.id(who), "ally_of", Direction::Both, Tick(t))
            .unwrap()
            .iter()
            .map(|(n, _)| self.label(*n))
            .collect();
        l.sort();
        l
    }

    /// The slice-5 entity object (`asof --json`'s `entity`) at a tick and canon.
    fn entity_obj(&self, h: &str, t: i64, c: u64) -> Json {
        let o = self.run(
            "asof",
            &[h, &format!("@{t}"), "--canon", &c.to_string(), "--json"],
        );
        assert_eq!(code(&o), 0, "{}", stderr(&o));
        let j: Json = serde_json::from_str(stdout(&o).trim()).unwrap();
        j["entity"].clone()
    }

    fn write_file(&self, name: &str, text: &str) -> PathBuf {
        let p = self.dir.join(name);
        std::fs::write(&p, text).unwrap();
        p
    }
}

/// Parses the text output: (header, columns line, row lines).
fn text_parts(out: &str) -> (String, String, Vec<String>) {
    let mut it = out.lines();
    let header = it.next().expect("header line").to_string();
    let cols = it.next().expect("column line").to_string();
    let rows: Vec<String> = it.map(str::to_string).collect();
    (header, cols, rows)
}

fn sorted_rows(rows: &[String]) -> Vec<String> {
    let mut v: Vec<String> = rows
        .iter()
        .map(|r| {
            assert!(r.starts_with("  "), "row not indented by two spaces: {r:?}");
            r[2..].to_string()
        })
        .collect();
    v.sort();
    v
}

fn json_ref_labels(j: &Json) -> Vec<String> {
    let mut v: Vec<String> = j["rows"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| {
            assert_eq!(r.as_array().unwrap().len(), 1);
            assert_eq!(r[0]["type"], json!("ref"));
            r[0]["value"]["label"].as_str().unwrap().to_string()
        })
        .collect();
    v.sort();
    v
}

fn plural(n: usize) -> String {
    format!("{n} {}", if n == 1 { "row" } else { "rows" })
}

fn wait_or_kill(mut child: std::process::Child, secs: u64, what: &str) -> std::process::ExitStatus {
    let deadline = Instant::now() + Duration::from_secs(secs);
    loop {
        if let Some(st) = child.try_wait().unwrap() {
            return st;
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("{what}: did not finish within {secs} s (hang)");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

// ---------------------------------------------------------------------------
// text output
// ---------------------------------------------------------------------------

#[test]
fn query_edwins_allies_text_matches_native_neighbors() {
    let fx = varrow();
    let want = fx.allies("edwin", 700, RETCON);
    assert!(!want.is_empty(), "fixture sanity: edwin has allies at 700");
    let o = fx.q(&[ALLIES, "--param", "who=edwin"]);
    assert_eq!(code(&o), 0, "stderr: {}", stderr(&o));
    assert_eq!(stderr(&o), "");
    let (header, cols, rows) = text_parts(&stdout(&o));
    assert_eq!(
        header,
        format!(
            "query at tick 700, canon tx#{RETCON}: {}",
            plural(want.len())
        )
    );
    assert_eq!(cols, "  ?a");
    assert_eq!(sorted_rows(&rows), want);
}

#[test]
fn query_param_name_with_question_mark_is_accepted() {
    let fx = varrow();
    let plain = fx.q(&[ALLIES, "--param", "who=edwin"]);
    let q = fx.q(&[ALLIES, "--param", "?who=edwin"]);
    assert_eq!(code(&q), 0, "stderr: {}", stderr(&q));
    assert_eq!(stdout(&q), stdout(&plain));
}

#[test]
fn query_entity_param_by_id_equals_by_handle() {
    let fx = varrow();
    let id = fx.twin.format_id(fx.id("edwin"));
    let by_handle = fx.q(&[ALLIES, "--param", "who=edwin"]);
    // `:kala_handle` binds ?who as a string, so for an entity-typed param use ?e directly.
    let by_id = fx.q(&[
        "[:find ?a :in $ ?e :where [?e :ally_of ?a]]",
        "--param",
        &format!("e={id}"),
    ]);
    let by_h = fx.q(&[
        "[:find ?a :in $ ?e :where [?e :ally_of ?a]]",
        "--param",
        "e=edwin",
    ]);
    assert_eq!(code(&by_id), 0, "stderr: {}", stderr(&by_id));
    assert_eq!(code(&by_h), 0, "stderr: {}", stderr(&by_h));
    assert_eq!(stdout(&by_id), stdout(&by_h));
    let (_, _, rows) = text_parts(&stdout(&by_id));
    assert_eq!(sorted_rows(&rows), fx.allies("edwin", 700, RETCON));
    let _ = by_handle;
}

#[test]
fn query_at_two_ticks_matches_native_and_header_reports_tick() {
    let fx = varrow();
    for t in [-210i64, -200] {
        let q =
            format!("[:find ?a :in $ ?who :at {t} :where [?e :kala_handle ?who] [?e :ally_of ?a]]");
        let want = fx.allies("edwin", t, RETCON);
        let o = fx.q(&[&q, "--param", "who=edwin"]);
        assert_eq!(code(&o), 0, "t={t}: {}", stderr(&o));
        let (header, _, rows) = text_parts(&stdout(&o));
        let expect_header = if want.is_empty() {
            format!("query at tick {t}, canon tx#{RETCON}: 0 rows")
        } else {
            format!(
                "query at tick {t}, canon tx#{RETCON}: {}",
                plural(want.len())
            )
        };
        assert_eq!(header, expect_header);
        if want.is_empty() {
            assert_eq!(rows, vec!["  (no rows)".to_string()]);
        } else {
            assert_eq!(sorted_rows(&rows), want, "t={t}");
        }
    }
    // the two moments really differ, or the test proves little
    assert_ne!(
        fx.allies("edwin", -210, RETCON),
        fx.allies("edwin", -200, RETCON),
        "fixture sanity: -210 and -200 differ"
    );
}

#[test]
fn query_at_param_tick_accepts_plus_sign_and_negative() {
    let fx = varrow();
    let q = "[:find ?a :in $ ?e ?t :at ?t :where [?e :ally_of ?a]]";
    for (arg, t) in [("+5", 5i64), ("-210", -210), ("0", 0)] {
        let o = fx.q(&[q, "--param", "e=edwin", "--param", &format!("t={arg}")]);
        assert_eq!(code(&o), 0, "t={arg}: {}", stderr(&o));
        let (header, _, rows) = text_parts(&stdout(&o));
        let want = fx.allies("edwin", t, RETCON);
        assert!(
            header.starts_with(&format!("query at tick {t}, canon tx#{RETCON}: ")),
            "{header}"
        );
        if !want.is_empty() {
            assert_eq!(sorted_rows(&rows), want);
        }
    }
}

#[test]
fn query_older_canon_literal_and_param() {
    let fx = varrow();
    let tick = fx.twin.at_canon(TxId(BEFORE_RETCON)).latest_tick().0;
    let want = fx.allies("edwin", tick, BEFORE_RETCON);
    let lit = "[:find ?a :in $ ?who :canon 11 :where [?e :kala_handle ?who] [?e :ally_of ?a]]";
    let par = "[:find ?a :in $ ?who ?c :canon ?c :where [?e :kala_handle ?who] [?e :ally_of ?a]]";
    let a = fx.q(&[lit, "--param", "who=edwin"]);
    assert_eq!(code(&a), 0, "{}", stderr(&a));
    let (h, _, rows) = text_parts(&stdout(&a));
    assert!(
        h.starts_with(&format!("query at tick {tick}, canon tx#{BEFORE_RETCON}: ")),
        "{h}"
    );
    assert_eq!(sorted_rows(&rows), want);
    for c in ["tx#11", "11"] {
        let b = fx.q(&[par, "--param", "who=edwin", "--param", &format!("c={c}")]);
        assert_eq!(code(&b), 0, "c={c}: {}", stderr(&b));
        assert_eq!(stdout(&b), stdout(&a), "c={c}");
    }
}

#[test]
fn query_default_canon_is_latest_and_tick_is_latest_tick() {
    let fx = varrow();
    let o = fx.q(&["[:find ?n :where [:edwin :name ?n]]"]);
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    let (h, cols, rows) = text_parts(&stdout(&o));
    assert_eq!(h, format!("query at tick 700, canon tx#{RETCON}: 1 row"));
    assert_eq!(cols, "  ?n");
    assert_eq!(rows, vec!["  \"the Grey King\"".to_string()]);
}

#[test]
fn query_empty_result_says_no_rows() {
    let fx = varrow();
    let q = "[:find ?a :in $ ?e :at -1000 :where [?e :ally_of ?a]]";
    let o = fx.q(&[q, "--param", "e=edwin"]);
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    let (h, cols, rows) = text_parts(&stdout(&o));
    assert_eq!(h, format!("query at tick -1000, canon tx#{RETCON}: 0 rows"));
    assert_eq!(cols, "  ?a");
    assert_eq!(rows, vec!["  (no rows)".to_string()]);
}

#[test]
fn query_typed_values_in_text_and_json() {
    let fx = typed();
    let o =
        fx.q(&["[:find ?n ?a ?b :where [:alice :name ?n] [:alice :age ?a] [:alice :brave ?b]]"]);
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    let (h, cols, rows) = text_parts(&stdout(&o));
    assert_eq!(
        h,
        "query at tick 5, canon tx#2: 1 row"
            .replace("tx#2", &format!("tx#{}", fx.twin.latest_tx().0))
    );
    assert_eq!(cols, "  ?n  ?a  ?b");
    assert_eq!(rows, vec!["  \"Alice\"  -5  true".to_string()]);

    let j =
        fx.json(&["[:find ?n ?a ?b :where [:alice :name ?n] [:alice :age ?a] [:alice :brave ?b]]"]);
    assert_eq!(j["command"], json!("query"));
    assert_eq!(j["columns"], json!(["?n", "?a", "?b"]));
    assert_eq!(
        j["rows"],
        json!([[
            {"type": "string", "value": "Alice"},
            {"type": "i64", "value": -5},
            {"type": "bool", "value": true}
        ]])
    );
}

#[test]
fn query_value_param_is_parsed_like_find_values() {
    let fx = typed();
    // i64 param
    let q = "[:find ?e :in $ ?a :where [?e :age ?a]]";
    let o = fx.q(&[q, "--param", "a=-5"]);
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    let (h, _, rows) = text_parts(&stdout(&o));
    assert!(h.ends_with(": 1 row"), "{h}");
    assert_eq!(rows, vec!["  alice".to_string()]);
    // bool param
    let o = fx.q(&[
        "[:find ?e :in $ ?b :where [?e :brave ?b]]",
        "--param",
        "b=true",
    ]);
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    assert!(stdout(&o).contains("\n  alice"), "{}", stdout(&o));
    // string param, spaces and all
    let o = fx.q(&[
        "[:find ?e :in $ ?n :where [?e :name ?n]]",
        "--param",
        "n=Alice",
    ]);
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    assert!(stdout(&o).contains("\n  alice"), "{}", stdout(&o));
}

// ---------------------------------------------------------------------------
// JSON output
// ---------------------------------------------------------------------------

#[test]
fn query_json_shape_for_refs_uses_slice5_entity_objects() {
    let fx = varrow();
    let want = fx.allies("edwin", 700, RETCON);
    let j = fx.json(&[ALLIES, "--param", "who=edwin"]);
    assert_eq!(j["command"], json!("query"));
    assert_eq!(j["canon"], json!(RETCON));
    assert_eq!(j["tick"], json!(700));
    assert_eq!(j["columns"], json!(["?a"]));
    assert_eq!(json_ref_labels(&j), want);
    for r in j["rows"].as_array().unwrap() {
        let h = r[0]["value"]["handle"].as_str().unwrap();
        assert_eq!(
            r[0]["value"],
            fx.entity_obj(h, 700, RETCON),
            "entity object of {h}"
        );
        assert_eq!(r[0]["value"]["id"], json!(fx.twin.format_id(fx.id(h))));
    }
    let keys: Vec<&str> = j.as_object().unwrap().keys().map(String::as_str).collect();
    for k in ["command", "canon", "tick", "columns", "rows"] {
        assert!(keys.contains(&k), "missing key {k}");
    }
}

#[test]
fn query_json_older_canon_and_ticks() {
    let fx = varrow();
    let q = "[:find ?a :in $ ?e :at -210 :canon 11 :where [?e :ally_of ?a]]";
    let j = fx.json(&[q, "--param", "e=edwin"]);
    assert_eq!(j["canon"], json!(BEFORE_RETCON));
    assert_eq!(j["tick"], json!(-210));
    assert_eq!(json_ref_labels(&j), fx.allies("edwin", -210, BEFORE_RETCON));
}

#[test]
fn query_json_empty_is_empty_rows_array() {
    let fx = varrow();
    let j = fx.json(&[
        "[:find ?a :in $ ?e :at -1000 :where [?e :ally_of ?a]]",
        "--param",
        "e=edwin",
    ]);
    assert_eq!(j["rows"], json!([]));
    assert_eq!(j["columns"], json!(["?a"]));
}

// ---------------------------------------------------------------------------
// QUERY vs -f
// ---------------------------------------------------------------------------

#[test]
fn query_from_file_matches_inline() {
    let fx = varrow();
    let f = fx.write_file("q.edn", ALLIES);
    let inline = fx.q(&[ALLIES, "--param", "who=edwin"]);
    let o = fx.q(&["-f", f.to_str().unwrap(), "--param", "who=edwin"]);
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    assert_eq!(stdout(&o), stdout(&inline));
}

#[test]
fn query_needs_exactly_one_of_query_or_file() {
    let fx = varrow();
    let f = fx.write_file("q.edn", ALLIES);
    let both = fx.q(&[ALLIES, "-f", f.to_str().unwrap(), "--param", "who=edwin"]);
    assert_eq!(code(&both), 2, "both: {}", stderr(&both));
    let neither = fx.q(&[]);
    assert_eq!(code(&neither), 2, "neither: {}", stderr(&neither));
    {
        let cmd = "explain";
        let b = fx.run(
            cmd,
            &[ALLIES, "-f", f.to_str().unwrap(), "--param", "who=edwin"],
        );
        assert_eq!(code(&b), 2, "{cmd} both");
        let n = fx.run(cmd, &[]);
        assert_eq!(code(&n), 2, "{cmd} neither");
    }
}

#[test]
fn query_file_size_limit_boundary() {
    let fx = varrow();
    let base = "[:find ?n :where [:edwin :name ?n]]";
    // exactly MAX_QUERY_BYTES (65536): accepted
    let ok = format!("{base}{}", " ".repeat(65536 - base.len()));
    assert_eq!(ok.len(), 65536);
    let f = fx.write_file("ok.edn", &ok);
    let o = fx.q(&["-f", f.to_str().unwrap()]);
    assert_eq!(code(&o), 0, "at the limit: {}", stderr(&o));
    // one byte more: exit 1
    let big = format!("{ok} ");
    let f = fx.write_file("big.edn", &big);
    let o = fx.q(&["-f", f.to_str().unwrap()]);
    assert_eq!(code(&o), 1, "over the limit");
    assert_eq!(stdout(&o), "");
    assert!(stderr(&o).starts_with("kala: error: "), "{}", stderr(&o));
}

#[test]
fn query_file_missing_or_directory_exits_1_promptly() {
    let fx = varrow();
    let missing = fx.dir.join("nope.edn");
    let o = fx.q(&["-f", missing.to_str().unwrap()]);
    assert_eq!(code(&o), 1);
    assert_eq!(stdout(&o), "");

    let child = Command::new(env!("CARGO_BIN_EXE_kala"))
        .args(["query"])
        .arg(&fx.path)
        .arg("-f")
        .arg(&fx.dir)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let st = wait_or_kill(child, 10, "-f on a directory");
    assert_eq!(st.code(), Some(1));
}

#[cfg(unix)]
#[test]
fn query_file_fifo_exits_1_promptly() {
    let fx = varrow();
    let fifo = fx.dir.join("q.fifo");
    let ok = Command::new("mkfifo")
        .arg(&fifo)
        .status()
        .map(|s| s.success());
    if !matches!(ok, Ok(true)) {
        eprintln!("skipped: no mkfifo");
        return;
    }
    let child = Command::new(env!("CARGO_BIN_EXE_kala"))
        .args(["query"])
        .arg(&fx.path)
        .arg("-f")
        .arg(&fifo)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let st = wait_or_kill(child, 10, "-f on a FIFO");
    assert_eq!(st.code(), Some(1));
}

// ---------------------------------------------------------------------------
// params
// ---------------------------------------------------------------------------

#[test]
fn query_param_errors_exit_1_with_nothing_on_stdout() {
    let fx = varrow();
    let tick_q = "[:find ?a :in $ ?e ?t :at ?t :where [?e :ally_of ?a]]";
    let canon_q = "[:find ?a :in $ ?e ?c :canon ?c :where [?e :ally_of ?a]]";
    let cases: Vec<(&str, Vec<&str>)> = vec![
        ("missing", vec![ALLIES]),
        (
            "unknown name",
            vec![ALLIES, "--param", "who=edwin", "--param", "zzz=1"],
        ),
        (
            "duplicate",
            vec![ALLIES, "--param", "who=edwin", "--param", "who=mara"],
        ),
        (
            "duplicate with and without ?",
            vec![ALLIES, "--param", "who=edwin", "--param", "?who=mara"],
        ),
        (
            "bad tick",
            vec![tick_q, "--param", "e=edwin", "--param", "t=abc"],
        ),
        (
            "float tick",
            vec![tick_q, "--param", "e=edwin", "--param", "t=1.5"],
        ),
        (
            "bad canon",
            vec![canon_q, "--param", "e=edwin", "--param", "c=abc"],
        ),
        (
            "negative canon",
            vec![canon_q, "--param", "e=edwin", "--param", "c=-1"],
        ),
        (
            "unknown entity",
            vec![canon_q, "--param", "e=nobody", "--param", "c=1"],
        ),
        (
            "bad entity id",
            vec![
                "[:find ?a :in $ ?e :where [?e :ally_of ?a]]",
                "--param",
                "e=CHARACTER#9999",
            ],
        ),
    ];
    for (what, args) in cases {
        let o = fx.q(&args);
        assert_eq!(code(&o), 1, "{what}: stderr {}", stderr(&o));
        assert_eq!(stdout(&o), "", "{what}");
        assert!(
            stderr(&o).starts_with("kala: error: "),
            "{what}: {}",
            stderr(&o)
        );
    }
}

#[test]
fn query_value_param_of_wrong_type_exits_1() {
    let fx = typed();
    let o = fx.q(&[
        "[:find ?e :in $ ?a :where [?e :age ?a]]",
        "--param",
        "a=abc",
    ]);
    assert_eq!(code(&o), 1, "{}", stderr(&o));
    assert_eq!(stdout(&o), "");
    let o = fx.q(&[
        "[:find ?e :in $ ?b :where [?e :brave ?b]]",
        "--param",
        "b=maybe",
    ]);
    assert_eq!(code(&o), 1, "{}", stderr(&o));
}

// ---------------------------------------------------------------------------
// Datalog errors
// ---------------------------------------------------------------------------

fn assert_query_error(o: &std::process::Output, what: &str) -> String {
    assert_eq!(code(o), 1, "{what}: stderr {}", stderr(o));
    assert_eq!(stdout(o), "", "{what}: nothing on stdout");
    let e = stderr(o);
    assert!(e.starts_with("kala: error: query: line "), "{what}: {e:?}");
    no_raw_controls(&e);
    e
}

#[test]
fn query_syntax_error_reports_line_and_column() {
    let fx = varrow();
    let o = fx.q(&["[:find ?a\n :where [?e :ally_of ?a"]);
    let e = assert_query_error(&o, "unclosed vector");
    // line and column are numbers
    let rest = &e["kala: error: query: line ".len()..];
    let (l, rest) = rest.split_once(", column ").expect("`, column `");
    assert!(
        l.chars().all(|c| c.is_ascii_digit()) && !l.is_empty(),
        "{e:?}"
    );
    let (c, _) = rest.split_once(':').expect("colon after column");
    assert!(
        c.chars().all(|c| c.is_ascii_digit()) && !c.is_empty(),
        "{e:?}"
    );
}

#[test]
fn query_unknown_attribute_and_unsupported_exit_1() {
    let fx = varrow();
    let o = fx.q(&["[:find ?a :where [:edwin :no_such_attr ?a]]"]);
    assert_query_error(&o, "unknown attribute");
    let o = fx.q(&[
        "[:find ?a :where [:edwin :ally_of ?a] (or-join [?a] [?a :name \"x\"] [?a :name \"y\"])]",
    ]);
    assert_query_error(&o, "unsupported or-join");
    let o =
        fx.q(&["[:find ?a :where [:edwin :ally_of ?a] (or [?a :name \"x\"] [?a :name \"y\"])]"]);
    assert_eq!(code(&o), 0, "or is now supported: {}", stderr(&o));
    assert!(stdout(&o).contains("0 rows"));
    let o = fx.q(&["[:find ?z :where [:edwin :ally_of ?a]]"]);
    assert_query_error(&o, "unbound find var");
}

#[test]
fn query_error_echo_is_escaped() {
    let fx = varrow();
    let q = "[:find ?a :where [:edwin :no\u{202e}pe ?a]]";
    let o = fx.q(&[q]);
    let e = assert_query_error(&o, "bidi in attribute");
    assert!(e.contains("\\u{202e}"), "{e:?}");
    assert!(!e.contains('\u{202e}'), "raw RLO in {e:?}");
    // also on explain
    let o = fx.run("explain", &[q]);
    let e = assert_query_error(&o, "explain bidi");
    assert!(e.contains("\\u{202e}") && !e.contains('\u{202e}'), "{e:?}");
}

#[test]
fn query_too_long_query_text_on_command_line_is_an_error_not_a_crash() {
    let fx = varrow();
    let big = format!("[:find ?n :where [:edwin :name ?n]]{}", " ".repeat(65537));
    let o = fx.q(&[&big]);
    assert_eq!(code(&o), 1, "{}", stderr(&o));
    assert_eq!(stdout(&o), "");
}

// ---------------------------------------------------------------------------
// explain
// ---------------------------------------------------------------------------

#[test]
fn explain_prints_the_plan_without_executing() {
    let fx = varrow();
    let o = fx.run("explain", &[ALLIES, "--param", "who=edwin"]);
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    assert_eq!(stderr(&o), "");
    let out = stdout(&o);
    assert!(out.starts_with("Find ?a"), "{out:?}");
    assert!(out.contains("Expand ["), "{out:?}");
    assert!(out.contains("via handle"), "{out:?}");
    // not a result listing
    assert!(!out.contains("query at tick"), "{out:?}");
    no_raw_controls(&out);
}

#[test]
fn explain_from_file_matches_inline() {
    let fx = varrow();
    let f = fx.write_file("q.edn", ALLIES);
    let a = fx.run("explain", &[ALLIES, "--param", "who=edwin"]);
    let b = fx.run(
        "explain",
        &["-f", f.to_str().unwrap(), "--param", "who=edwin"],
    );
    assert_eq!(code(&b), 0, "{}", stderr(&b));
    assert_eq!(stdout(&a), stdout(&b));
}

#[test]
fn explain_param_errors_exit_1() {
    let fx = varrow();
    let o = fx.run("explain", &[ALLIES]);
    assert_eq!(code(&o), 1, "missing param: {}", stderr(&o));
    assert_eq!(stdout(&o), "");
    let o = fx.run(
        "explain",
        &[ALLIES, "--param", "who=edwin", "--param", "zzz=1"],
    );
    assert_eq!(code(&o), 1, "unknown param");
    let o = fx.run("explain", &["[:find ?a :where [:edwin :nope ?a]]"]);
    assert_query_error(&o, "explain unknown attribute");
}

// ---------------------------------------------------------------------------
// output failures
// ---------------------------------------------------------------------------

#[cfg(target_os = "linux")]
#[test]
fn query_and_explain_stdout_to_dev_full_exit_3() {
    let Ok(_) = std::fs::OpenOptions::new().write(true).open("/dev/full") else {
        eprintln!("skipped: no /dev/full");
        return;
    };
    let fx = varrow();
    for (cmd, json) in [("query", false), ("query", true), ("explain", false)] {
        let full = std::fs::OpenOptions::new()
            .write(true)
            .open("/dev/full")
            .unwrap();
        let mut c = Command::new(env!("CARGO_BIN_EXE_kala"));
        c.arg(cmd)
            .arg(&fx.path)
            .arg(ALLIES)
            .args(["--param", "who=edwin"]);
        if json {
            c.arg("--json");
        }
        let o = c.stdout(full).stderr(Stdio::piped()).output().unwrap();
        assert_eq!(
            o.status.code(),
            Some(3),
            "{cmd} json={json}: {}",
            stderr(&o)
        );
    }
}

#[test]
fn query_does_not_modify_the_world_file() {
    let fx = varrow();
    let before = std::fs::read(&fx.path).unwrap();
    let _ = fx.q(&[ALLIES, "--param", "who=edwin"]);
    let _ = fx.run("explain", &[ALLIES, "--param", "who=edwin"]);
    assert_eq!(std::fs::read(&fx.path).unwrap(), before);
}

// ---------------------------------------------------------------------------
// history patterns (slice 7, item 5)
// ---------------------------------------------------------------------------

#[test]
fn query_history_pattern_prints_ops_in_the_op_column() {
    let fx = varrow();
    // Greg's location change points on varrow.yaml (see kala-datalog's
    // history_patterns.rs for the derivation): -100/150 carnath, 200/250
    // varrow, 601 ashford.
    let o = fx.q(&["[:find ?t ?op :where [:greg :located_in ?p ?t ?op]]"]);
    assert_eq!(code(&o), 0, "stderr: {}", stderr(&o));
    let (_, cols, rows) = text_parts(&stdout(&o));
    assert_eq!(cols.split_whitespace().collect::<Vec<_>>(), ["?t", "?op"]);
    let cells: Vec<Vec<String>> = sorted_rows(&rows)
        .iter()
        .map(|r| r.split_whitespace().map(str::to_string).collect())
        .collect();
    let mut want: Vec<Vec<String>> = [
        ("-100", ":assert"),
        ("150", ":retract"),
        ("200", ":assert"),
        ("250", ":retract"),
        ("601", ":assert"),
    ]
    .iter()
    .map(|(t, op)| vec![t.to_string(), op.to_string()])
    .collect();
    want.sort();
    let mut got = cells;
    got.sort();
    assert_eq!(got, want);
}

#[test]
fn cypher_language_dispatch_json_shape_and_datalog_default() {
    let f = varrow();
    let datalog =
        f.json(&["[:find ?a :at -210 :where [?e :kala_handle \"edwin\"] [?e :ally_of ?a]]"]);
    let cypher = f.json(&[
        "--lang",
        "cypher",
        "MATCH (e {kala_handle: 'edwin'})-[:ally_of]->(a) AT TICK -210 RETURN DISTINCT a",
    ]);
    assert_eq!(cypher, datalog);
    let explicit = f.json(&[
        "--lang",
        "datalog",
        "[:find ?a :at -210 :where [?e :kala_handle \"edwin\"] [?e :ally_of ?a]]",
    ]);
    assert_eq!(explicit, datalog);
    let expected = f.run(
        "explain",
        &["[:find ?a :at -210 :where [?e :kala_handle \"edwin\"] [?e :ally_of ?a]]"],
    );
    let actual = f.run(
        "explain",
        &[
            "--lang",
            "cypher",
            "MATCH (e {kala_handle: 'edwin'})-[:ally_of]->(a) AT TICK -210 RETURN DISTINCT a",
        ],
    );
    assert_eq!(code(&actual), 0, "{}", stderr(&actual));
    assert_eq!(stdout(&actual), stdout(&expected));
}

#[test]
fn cypher_cli_parameters_order_null_list_and_positioned_errors() {
    let f = varrow();
    let names = f.json(&[
        "--lang",
        "cypher",
        "MATCH (c:CHARACTER) RETURN c.name ORDER BY c.name SKIP $s LIMIT $n",
        "--param",
        "s=2",
        "--param",
        "n=2",
    ]);
    let text = names.to_string();
    assert!(text.contains("Hild") && text.contains("Lisa"), "{text}");
    let result = f.q(&[
        "--lang",
        "cypher",
        "MATCH (c:CHARACTER) RETURN c.name ORDER BY c.name DESC LIMIT 3",
    ]);
    assert_eq!(code(&result), 0, "{}", stderr(&result));
    let text = stdout(&result);
    assert!(text.find("the Grey King").unwrap() < text.find("Wren Åberg").unwrap());
    assert!(text.find("Wren Åberg").unwrap() < text.find("Villager 24").unwrap());
    let nulls = f.json(&[
        "--lang",
        "cypher",
        "MATCH (c {kala_handle: 'kael'}) RETURN c.biography",
    ]);
    assert!(nulls.to_string().contains("null"));
    let aliases = f.json(&[
        "--lang",
        "cypher",
        "MATCH (e {kala_handle: 'edwin'}) AT TICK -210 RETURN e.alias",
    ]);
    assert!(aliases.to_string().contains("the Lost Knight"));
    let syntax = f.q(&["--lang", "cypher", "MATCH (c) RETURN properties(c)"]);
    assert_ne!(code(&syntax), 0);
    assert!(stderr(&syntax).contains("properties"));
    assert!(stderr(&syntax).contains("line") && stderr(&syntax).contains("column"));
    let missing = f.q(&["--lang", "cypher", "MATCH (c) AT TICK $t RETURN c"]);
    assert_ne!(code(&missing), 0);
    let bad = f.q(&[
        "--lang",
        "cypher",
        "MATCH (c) AT TICK $t RETURN c",
        "--param",
        "t=wrong",
    ]);
    assert_ne!(code(&bad), 0);
}

#[test]
fn cypher_cli_json_list_and_null_parameters() {
    let f = varrow();
    let aliases = f.json(&[
        "--lang", "cypher",
        "MATCH (e {kala_handle: 'edwin'}) AT TICK -210 WHERE e.alias = $aliases RETURN e.kala_handle",
        "--param", r#"aliases=["Ed","Eddie","the Lost Knight"]"#,
    ]);
    assert_eq!(aliases["rows"].as_array().unwrap().len(), 1);
    assert_eq!(aliases["rows"][0][0]["value"], "edwin");
    let null = f.json(&[
        "--lang",
        "cypher",
        "MATCH (e {kala_handle: 'edwin'}) RETURN $x",
        "--param",
        "x=null",
    ]);
    assert_eq!(null["rows"].as_array().unwrap().len(), 1);
    assert_eq!(null["rows"][0][0]["type"], "null");
    let nested = f.json(&[
        "--lang",
        "cypher",
        "MATCH (e {kala_handle: 'edwin'}) RETURN $x",
        "--param",
        r#"x=[[1,null],["x",true]]"#,
    ]);
    assert_eq!(nested["rows"].as_array().unwrap().len(), 1);
    assert_eq!(
        nested["rows"][0][0],
        json!({"type":"list","value":[
            {"type":"list","value":[{"type":"i64","value":1},{"type":"null","value":null}]},
            {"type":"list","value":[{"type":"string","value":"x"},{"type":"bool","value":true}]}
        ]})
    );
}

const HEIGHTS: &str = r#"
schema:
  node_types: [CHARACTER]
  attributes:
    - { name: name, type: string }
    - { name: height, type: f64 }
entities:
  CHARACTER: [alice]
transactions:
  - tick: 0
    ops:
      - assert: [alice, name, "Alice"]
      - assert: [alice, height, 2.0]
"#;

fn heights() -> Fx {
    let dir = tempfile::tempdir().unwrap();
    let yaml = dir.path().join("h.yaml");
    std::fs::write(&yaml, HEIGHTS).unwrap();
    let path = dir.path().join("w.kala");
    let o = kala([
        OsString::from("load"),
        yaml.clone().into(),
        path.clone().into(),
    ]);
    assert_eq!(code(&o), 0, "kala load: {}", stderr(&o));
    Fx {
        dir: dir.path().to_path_buf(),
        _dir: dir,
        path,
        twin: load_fixture(HEIGHTS).unwrap(),
    }
}

#[test]
fn cypher_cli_f64_param_accepts_integer_text() {
    let f = heights();
    for text in ["2", "2.0", "+2", "2e0"] {
        let j = f.json(&[
            "--lang",
            "cypher",
            "MATCH (a) WHERE a.height = $p RETURN a.name",
            "--param",
            &format!("p={text}"),
        ]);
        assert_eq!(j["rows"].as_array().unwrap().len(), 1, "p={text}: {j}");
        assert_eq!(j["rows"][0][0]["value"], "Alice", "p={text}");
    }
}

fn scalar_echo(f: &Fx, text: &str) -> Json {
    let j = f.json(&[
        "--lang",
        "cypher",
        "MATCH (a {kala_handle: 'alice'}) RETURN $p",
        "--param",
        &format!("p={text}"),
    ]);
    assert_eq!(j["rows"].as_array().unwrap().len(), 1, "p={text}: {j}");
    j["rows"][0][0].clone()
}

#[test]
fn cypher_cli_scalar_param_non_numeric_words_stay_strings() {
    let f = heights();
    for text in ["nan", "inf", "Infinity", "-inf", "NaN", "+infinity"] {
        assert_eq!(
            scalar_echo(&f, text),
            json!({"type":"string","value":text}),
            "p={text}"
        );
    }
}

#[test]
fn cypher_cli_scalar_param_numeric_looking_text_is_a_float() {
    let f = heights();
    for (text, want) in [("1.5", 1.5), ("-2e3", -2000.0), (".5", 0.5), ("2.", 2.0)] {
        assert_eq!(
            scalar_echo(&f, text),
            json!({"type":"f64","value":want}),
            "p={text}"
        );
    }
    assert_eq!(scalar_echo(&f, "7"), json!({"type":"i64","value":7}));
}
