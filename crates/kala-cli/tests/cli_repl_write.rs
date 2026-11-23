//! Write sessions hold one lock, commit atomically and read their latest canon.
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
    child
        .stdin
        .take()
        .unwrap()
        .write_all(text.as_bytes())
        .unwrap();
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

#[test]
fn write_mode_holds_lock_until_quit_and_allows_readers() {
    let f = build(EMPTY);
    let mut live = Live::new(&f);
    live.send(".json on\n[:find ?a :where [:greg :age ?a]]\n");
    assert!(live.result()["rows"].as_array().unwrap().is_empty());
    assert!(matches!(
        World::open(&f.path),
        Err(kala_core::Error::WorldLocked)
    ));
    assert!(World::open_read_only(&f.path).is_ok());
    live.send("[:assert :greg :age 18 :at -300]\n");
    assert_eq!(live.result()["changed"], 1);
    let reader = World::open_read_only(&f.path).unwrap();
    assert_eq!(
        reader
            .as_of(f.id("greg"), "age", kala_core::Tick(-300))
            .unwrap(),
        [kala_core::Value::I64(18)]
    );
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
