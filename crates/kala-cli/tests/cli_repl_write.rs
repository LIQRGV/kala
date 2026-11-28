//! Write sessions take the writer lock per commit, commit atomically and read
//! the latest canon, other processes' commits included.
mod common;
use common::*;
use kala_core::World;
use serde_json::Value as Json;
use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Output, Stdio};

const EMPTY: &str = r#"
schema:
  node_types: [CHARACTER, PLACE]
  attributes:
    - { name: located_in, type: ref }
    - { name: name, type: string }
    - { name: age, type: i64 }
entities:
  CHARACTER: [greg]
  PLACE: [varrow, ashford]
"#;

fn repl(f: &Fx, text: &str, flags: &[&str]) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_kala"))
        .arg("repl")
        .arg(&f.path)
        .args(flags)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    feed(&mut child, text.as_bytes());
    child.wait_with_output().unwrap()
}

fn documents(o: &Output) -> Vec<Json> {
    serde_json::Deserializer::from_slice(&o.stdout)
        .into_iter()
        .map(Result::unwrap)
        .collect()
}

#[test]
fn read_only_write_refusal_has_hint_and_preserves_log() {
    let f = build(EMPTY);
    let before = std::fs::read(&f.path).unwrap();
    let o = repl(
        &f,
        "[:assert :greg :age 18]\n[:find ?a :where [:greg :age ?a]]\n",
        &[],
    );
    assert_eq!(code(&o), 1);
    assert!(stderr(&o).contains("start with --write"), "{}", stderr(&o));
    assert!(stdout(&o).contains("0 rows"), "{}", stdout(&o));
    assert_eq!(std::fs::read(&f.path).unwrap(), before);
}

#[test]
fn queries_see_commits_without_reload() {
    let f = build(EMPTY);
    let o = repl(
        &f,
        ".json on\n[:find ?a :where [:greg :age ?a]]\n[:assert :greg :age 18 :at -300]\n[:find ?a :where [:greg :age ?a]]\n.quit\n",
        &["--write"],
    );
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    let docs = documents(&o);
    assert_eq!(docs.len(), 3);
    assert!(docs[0]["rows"].as_array().unwrap().is_empty());
    assert_eq!(docs[1]["changed"], 1);
    assert_eq!(docs[2]["rows"][0][0]["value"], 18);
    assert_eq!(docs[2]["canon"], docs[1]["tx"]);
}

#[test]
fn repl_counts_inferred_and_explicit_schema_separately() {
    for json in [false, true] {
        let f = build(EMPTY);
        let source = format!(
            "{}[:assert :greg :height 180 :at -1]\n[:attribute :title :string]\n[:assert :greg :title \"Sir\" :at 0]\n.quit\n",
            if json { ".json on\n" } else { "" }
        );
        let output = repl(&f, &source, &["--write"]);
        assert_eq!(code(&output), 0, "{}", stderr(&output));
        if json {
            let docs = documents(&output);
            for (report, (changed, declared)) in docs.iter().zip([(1, 1), (0, 1), (1, 0)]) {
                assert_eq!(report["changed"], changed, "{report}");
                assert_eq!(report["declared"], declared, "{report}");
                assert_eq!(report["ops"][0]["changed"], changed, "{report}");
                assert_eq!(report["ops"][0]["declared"], declared, "{report}");
            }
            assert_eq!(docs.len(), 3);
        } else {
            let text = stdout(&output);
            assert!(text.contains("1 changed, 1 declared"), "{text}");
            assert!(text.contains("0 changed, 1 declared"), "{text}");
            let fact_report = text
                .lines()
                .rfind(|line| line.contains("committed tx#"))
                .unwrap();
            assert!(fact_report.ends_with("1 changed"), "{text}");
        }
    }
}

#[test]
fn at_defaults_follow_op_then_transaction_then_session_then_world() {
    let f = build(EMPTY);
    let o = repl(
        &f,
        ".json on\n.at -300\n[:assert :greg :age 18]\n[:at -200 [:assert :greg :name \"Greg\"] [:assert :greg :age 19 :at -100]]\n.at\n[:assert :greg :age 20]\n",
        &["--write"],
    );
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    let docs = documents(&o);
    assert_eq!(docs.len(), 3);
    assert_eq!(docs[0]["ops"][0]["tick"], -300);
    assert_eq!(docs[1]["ops"][0]["tick"], -200);
    assert_eq!(docs[1]["ops"][1]["tick"], -100);
    assert_eq!(docs[2]["ops"][0]["tick"], -100);
}

#[test]
fn canon_and_reload_are_refused_but_session_continues() {
    let f = build(EMPTY);
    let o = repl(
        &f,
        ".canon 1\n.reload\n[:assert :greg :age 18]\n[:find ?a :where [:greg :age ?a]]\n",
        &["--write"],
    );
    assert_eq!(code(&o), 1, "{}", stderr(&o));
    let err = stderr(&o);
    assert!(err.contains(".canon") && err.contains(".reload"), "{err}");
    assert!(
        stdout(&o).contains("1 changed") && stdout(&o).contains("18"),
        "{}",
        stdout(&o)
    );
}

#[test]
fn transaction_error_names_third_op_and_next_transaction_succeeds() {
    let f = build(EMPTY);
    let o = repl(
        &f,
        ".json on\n[[:create :CHARACTER :kael] [:assert :kael :age 18 :at 1] [:retract :kael :age 18 :at 1]]\n[:assert :greg :age 20 :at 2]\n[:find ?a :where [:greg :age ?a]]\n",
        &["--write"],
    );
    assert_eq!(code(&o), 1);
    assert!(
        stderr(&o).contains("op 3") && stderr(&o).contains(":retract"),
        "{}",
        stderr(&o)
    );
    let docs = documents(&o);
    assert_eq!(docs.len(), 2);
    assert_eq!(docs[0]["changed"], 1);
    assert_eq!(docs[1]["rows"][0][0]["value"], 20);
    assert!(
        World::open_read_only(&f.path)
            .unwrap()
            .entity_by_handle("kael")
            .is_none()
    );
}

#[test]
fn multiline_writes_and_cancel_share_query_completeness() {
    let f = build(EMPTY);
    let o = repl(
        &f,
        "[[:create :CHARACTER :kael]\n; an op may follow on another line\n[:assert :kael :name \"Kael\"]]\n[:assert :greg :name\n.cancel\n[:find ?n :where [:kael :name ?n]]\n",
        &["--write"],
    );
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    assert!(stdout(&o).contains("\"Kael\""), "{}", stdout(&o));
    assert!(
        World::open_read_only(&f.path)
            .unwrap()
            .history(f.id("greg"), "name")
            .unwrap()
            .is_empty()
    );
}

struct Live {
    child: std::process::Child,
    input: std::process::ChildStdin,
    lines: std::sync::mpsc::Receiver<String>,
}

impl Live {
    fn new(f: &Fx) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_kala"))
            .arg("repl")
            .arg(&f.path)
            .arg("--write")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let input = child.stdin.take().unwrap();
        let output = child.stdout.take().unwrap();
        let (send, lines) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(output).lines() {
                if send.send(line.unwrap()).is_err() {
                    break;
                }
            }
        });
        Self {
            child,
            input,
            lines,
        }
    }
    fn send(&mut self, text: &str) {
        self.input.write_all(text.as_bytes()).unwrap();
        self.input.flush().unwrap();
    }
    fn result(&self) -> Json {
        loop {
            let l = self
                .lines
                .recv_timeout(std::time::Duration::from_secs(20))
                .expect("REPL flushes a result without closing stdin");
            if let Ok(j) = serde_json::from_str::<Json>(&l) {
                return j;
            }
        }
    }
}

impl Drop for Live {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// `kala transact WORLD` with `text` as its forms, from another process.
fn transact(f: &Fx, text: &str) -> Output {
    let input = f.dir.path().join("other-writes.edn");
    std::fs::write(&input, text).unwrap();
    f.run("transact", &[input.to_str().unwrap(), "--json"])
}

/// Slice 15b: the writer lock is taken per transaction, so an open, idle
/// `kala repl --write` no longer keeps other writers out, and it sees their
/// commits on its next command. (Was
/// `write_mode_holds_lock_until_quit_and_allows_readers`, which pinned the
/// lifetime lock.)
#[test]
fn write_mode_leaves_the_lock_free_and_sees_other_commits() {
    let f = build(EMPTY);
    let mut live = Live::new(&f);
    live.send(".json on\n[:find ?a :where [:greg :age ?a]]\n");
    assert!(live.result()["rows"].as_array().unwrap().is_empty());
    assert!(World::open_read_only(&f.path).is_ok());

    // The REPL is open and idle: another process commits to the same world.
    let o = transact(&f, "[:assert :greg :age 18 :at -300]");
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    let other: Json = serde_json::from_slice(&o.stdout).unwrap();
    assert_eq!(other["changed"], 1);
    let other_tx = other["tx"].as_u64().unwrap();

    // The REPL's next query sees that commit, without a restart.
    live.send("[:find ?a :at -300 :where [:greg :age ?a]]\n");
    let rows = live.result()["rows"].clone();
    assert_eq!(rows.as_array().unwrap().len(), 1, "{rows}");
    assert!(rows.to_string().contains("18"), "{rows}");

    // And its next write lands right after it, on the state it left.
    live.send("[:assert :greg :age 18 :at -300]\n");
    let noop = live.result();
    assert_eq!(noop["changed"], 0, "already held since the other commit");
    live.send("[:assert :greg :age 19 :at -200]\n");
    let mine = live.result();
    assert_eq!(mine["changed"], 1);
    assert_eq!(mine["tx"].as_u64().unwrap(), other_tx + 1);

    // A write the REPL makes right after another process's commit, with no
    // query in between, catches up first too.
    let o = transact(&f, "[:assert :greg :name \"Greg\" :at 0]");
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    live.send("[:assert :greg :age 20 :at -100]\n");
    let mine = live.result();
    assert_eq!(mine["changed"], 1);
    assert_eq!(mine["tx"].as_u64().unwrap(), other_tx + 3);
    live.send("[:find ?n :at 0 :where [:greg :name ?n]]\n");
    assert_eq!(live.result()["rows"].as_array().unwrap().len(), 1);

    let reader = World::open_read_only(&f.path).unwrap();
    let greg = f.id("greg");
    for (tick, age) in [(-300, 18), (-200, 19), (-100, 20)] {
        assert!(
            reader
                .as_of(greg, "age", kala_core::Tick(tick))
                .unwrap()
                .contains(&kala_core::Value::I64(age)),
            "age {age} at {tick}"
        );
    }
    // The store is sound (the three ages overlap: a schema warning, as
    // for one writer).
    let report = kala_core::doctor::doctor_log(&f.path, &Default::default()).unwrap();
    assert!(report.errors().is_empty(), "{report:?}");
    live.send(".quit\n");
    assert!(live.child.wait().unwrap().success());
    assert!(World::open(&f.path).is_ok());
}

#[test]
fn initial_at_flag_applies_to_writes_and_precision_prints() {
    let f = build(EMPTY);
    let o = repl(
        &f,
        "[:assert :greg :name \"Greg\" :precision :year]\n[:find ?p :where [:greg :name _ _ _ ?p]]\n",
        &["--write", "--at", "-300"],
    );
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    assert!(
        stdout(&o).contains("-300") && stdout(&o).contains(":year"),
        "{}",
        stdout(&o)
    );
}

#[test]
fn session_rules_can_use_or_and_observe_later_commits() {
    let f = build(EMPTY);
    let o = repl(
        &f,
        ".json on\n.rule [[(named ?c) (or [?c :name \"Greg\"] [?c :name \"Kael\"])]]\n[:assert :greg :name \"Greg\"]\n[:find ?c :where (named ?c)]\n[[:create :CHARACTER :kael] [:assert :kael :name \"Kael\"]]\n[:find ?c :where (named ?c)]\n",
        &["--write"],
    );
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    let docs = documents(&o);
    assert_eq!(docs.len(), 4);
    assert_eq!(docs[1]["rows"].as_array().unwrap().len(), 1);
    assert_eq!(docs[3]["rows"].as_array().unwrap().len(), 2);
    let mut handles = docs[3]["rows"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r[0]["value"]["handle"].as_str().unwrap())
        .collect::<Vec<_>>();
    handles.sort();
    assert_eq!(handles, ["greg", "kael"]);
}

// PR #3 review round 1: a malformed query keeps the parser's own message.
#[test]
fn malformed_query_keeps_the_parsers_specific_error() {
    let f = build(EMPTY);
    let o = repl(&f, "[:where [?e :name ?n]]\n", &[]);
    assert!(stderr(&o).contains(":find"), "{}", stderr(&o));
    assert!(!stderr(&o).contains("or a write"), "{}", stderr(&o));
}

#[test]
fn bulk_params_repl_uses_dot_params_for_datalog_writes() {
    let f = build(EMPTY);
    let output = repl(
        &f,
        ".param e greg\n.param v 37\n.param t -42\n[:assert ?e :age ?v :at ?t]\n.quit\n",
        &["--write"],
    );
    assert_eq!(code(&output), 0, "{}", stderr(&output));
    let w = World::open_read_only(&f.path).unwrap();
    let e = w.entity_by_handle("greg").unwrap();
    assert_eq!(
        w.as_of(e, "age", kala_core::Tick(-42)).unwrap(),
        [kala_core::Value::I64(37)]
    );
}

#[test]
fn bulk_params_string_for_an_undeclared_attribute_is_refused_by_transact_and_the_repl() {
    let fx = common::varrow();
    let before = std::fs::read(&fx.path).unwrap();
    let input = fx.dir.path().join("friends");
    std::fs::write(&input, "[:assert ?a :friend_of ?b :at 700]").unwrap();
    let args = [
        input.to_str().unwrap(),
        "--param",
        "a=greg",
        "--param",
        "b=edwin",
    ];
    let refused = fx.fails(1, "transact", &args);
    for part in [
        "?b",
        ":friend_of",
        "[:attribute :friend_of :ref]",
        ":string",
    ] {
        assert!(refused.contains(part), "missing {part}: {refused}");
    }
    assert_eq!(std::fs::read(&fx.path).unwrap(), before);
    let session = repl(
        &fx,
        ".param a greg\n.param b edwin\n[:assert ?a :friend_of ?b :at 700]\n.quit\n",
        &["--write"],
    );
    let shown = format!("{}{}", stdout(&session), stderr(&session));
    assert!(shown.contains("[:attribute :friend_of :ref]"), "{shown}");
    assert_eq!(std::fs::read(&fx.path).unwrap(), before);

    // Declared in an earlier form of the same file, the string is a handle.
    std::fs::write(
        &input,
        "[:attribute :friend_of :ref :many]\n[:assert ?a :friend_of ?b :at 700]",
    )
    .unwrap();
    let o = fx.run("transact", &args);
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    let w = kala_core::World::open_read_only(&fx.path).unwrap();
    let (greg, edwin) = (
        w.entity_by_handle("greg").unwrap(),
        w.entity_by_handle("edwin").unwrap(),
    );
    assert_eq!(
        w.as_of(greg, "friend_of", kala_core::Tick(700)).unwrap(),
        [kala_core::Value::Ref(edwin)]
    );
}
