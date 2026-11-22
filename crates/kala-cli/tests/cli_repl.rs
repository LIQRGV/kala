//! Read-only REPL end-to-end contract, through the same plain-input backend
//! used by scripts. No terminal emulation is involved.
mod common;

use common::*;
use serde_json::Value as Json;
use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Output, Stdio};

const ALLIES: &str = "[:find ?a :at -210 :where [:edwin :ally_of ?a]]";

fn repl(fx: &Fx, input: &[u8], flags: &[&str]) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_kala"))
        .arg("repl")
        .arg(&fx.path)
        .args(flags)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(input).unwrap();
    child.wait_with_output().unwrap()
}

#[test]
fn piped_query_matches_kala_query() {
    let fx = varrow();
    let expected = fx.run("query", &[ALLIES]);
    assert_eq!(code(&expected), 0, "{}", stderr(&expected));
    let actual = repl(&fx, format!("{ALLIES}\n").as_bytes(), &[]);
    assert_eq!(code(&actual), 0, "REPL failed: {}", stderr(&actual));
    assert_eq!(actual.stdout, expected.stdout);
    assert_eq!(stderr(&actual), "");
}

fn documents(output: &Output) -> Vec<Json> {
    serde_json::Deserializer::from_slice(&output.stdout)
        .into_iter()
        .map(Result::unwrap)
        .collect()
}

#[test]
fn multiline_comments_strings_and_cancel_share_one_loop() {
    let fx = varrow();
    let input = "[:find ?a\n; ] ignored\n:at -210\n\n:where [:edwin :ally_of ?a]]\n[:find ?x\n.cancel\n[:find ?x :where [?x :name \"a ] \\\" b\"]]\n";
    let o = repl(&fx, input.as_bytes(), &[]);
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    let out = stdout(&o);
    assert!(out.starts_with(&fx.text("query", &[ALLIES])), "{out}");
    assert_eq!(out.matches("query at tick").count(), 2);
    assert!(out.contains("0 rows"));
    assert!(!out.contains("kala>") && !out.contains("...>"));
}

#[test]
fn unmatched_closer_and_unknown_command_report_errors_and_keep_running() {
    let fx = varrow();
    let o = repl(
        &fx,
        format!("]\n.unknown\n{ALLIES}\n.quit\n.ignored\n").as_bytes(),
        &[],
    );
    assert_eq!(code(&o), 1);
    assert_eq!(stdout(&o), fx.text("query", &[ALLIES]));
    assert_eq!(
        stderr(&o)
            .lines()
            .filter(|l| l.starts_with("error:"))
            .count(),
        2
    );
    assert!(stderr(&o).contains(".help"), "{}", stderr(&o));
}

#[test]
fn session_defaults_fall_back_clear_and_report_actual_canon_and_tick() {
    let fx = varrow();
    let query = "[:find ?a :where [:edwin :ally_of ?a]]";
    let input = format!(
        ".json on\n.at -195\n.canon 11\n{query}\n[:find ?a :at -200 :canon 12 :where [:edwin :ally_of ?a]]\n.at\n.canon\n{query}\n"
    );
    let o = repl(&fx, input.as_bytes(), &[]);
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    let docs = documents(&o);
    assert_eq!(docs.len(), 3);
    assert_eq!(docs[0]["tick"], -195);
    assert_eq!(docs[0]["canon"], 11);
    assert_eq!(docs[0]["rows"].as_array().unwrap().len(), 3);
    assert_eq!(docs[1]["tick"], -200);
    assert_eq!(docs[1]["canon"], 12);
    assert_eq!(docs[1]["rows"].as_array().unwrap().len(), 2);
    assert_eq!(docs[2]["tick"], 700);
    assert_eq!(docs[2]["canon"], 12);
    let flags = repl(
        &fx,
        format!("{query}\n").as_bytes(),
        &["--at", "-195", "--canon", "tx#11"],
    );
    assert_eq!(code(&flags), 0, "{}", stderr(&flags));
    assert!(stdout(&flags).starts_with("query at tick -195, canon tx#11: 3 rows"));
}

#[test]
fn session_parameters_are_typed_unused_ignored_and_missing_rejected() {
    let fx = varrow();
    let query = "[:find ?a :in $ ?who :at -210 :where [?e :handle ?who] [?e :ally_of ?a]]";
    let input = format!(
        ".param ?who edwin\n.param unused ignored\n{query}\n.params\n.unparam who\n{query}\n{ALLIES}\n"
    );
    let o = repl(&fx, input.as_bytes(), &[]);
    assert_eq!(code(&o), 1);
    assert!(
        stderr(&o).contains("missing parameter `?who`"),
        "{}",
        stderr(&o)
    );
    assert_eq!(stdout(&o).matches("query at tick -210").count(), 2);
    assert!(stdout(&o).contains("edwin"));
    assert!(stdout(&o).contains("unused"));
}

#[test]
fn recursive_session_rules_and_query_overrides_match_acceptance() {
    let fx = varrow();
    let rule = ".rule [[(allied ?a ?b) [?a :ally_of ?b]]\n[(allied ?a ?b) [?a :ally_of ?x] (allied ?x ?b)]]";
    let query = "[:find ?b :at 600 :where (allied :edwin ?b)]";
    let own =
        "[:find ?b :at -210 :rules [[(allied ?a ?b) [?a :ally_of ?b]]] :where (allied :edwin ?b)]";
    let o = repl(
        &fx,
        format!("{rule}\n{query}\n{own}\n.rules\n.unrule allied\n{query}\n").as_bytes(),
        &[],
    );
    assert_eq!(code(&o), 1);
    let acceptance = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/acceptance/03-alliance-reachability.expected"
    ))
    .unwrap();
    assert!(stdout(&o).starts_with(&acceptance), "{}", stdout(&o));
    assert!(stdout(&o).contains("query at tick -210, canon tx#12: 2 rows"));
    assert!(stdout(&o).contains("allied"));
    assert!(
        stderr(&o).contains("no rule named `allied`"),
        "{}",
        stderr(&o)
    );
}

#[test]
fn inspection_commands_explain_load_json_and_exit() {
    let fx = varrow();
    let file = fx.dir.path().join("q.edn");
    std::fs::write(&file, ALLIES).unwrap();
    let input = format!(
        ".help\n.schema\n.limits\n.info\n.explain [:find ?a\n:at -210 :where [:edwin :ally_of ?a]]\n.load {}\n.json on\n{ALLIES}\n.json off\n{ALLIES}\n.exit\n.bad\n",
        file.display()
    );
    let o = repl(&fx, input.as_bytes(), &[]);
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    let out = stdout(&o);
    for word in [
        ".reload",
        ".param",
        "CHARACTER",
        "ally_of",
        "bidirectional",
        "indexed",
        "100000",
        "10000000",
    ] {
        assert!(out.contains(word), "missing {word}: {out}");
    }
    assert!(out.contains(&fx.text("info", &[])), "{out}");
    assert!(out.contains(&fx.text("explain", &[ALLIES])), "{out}");
    assert!(
        out.contains(&fx.text("query", &[ALLIES, "--json"])),
        "{out}"
    );
    assert_eq!(out.matches("query at tick -210").count(), 2);
}

#[test]
fn errors_and_values_escape_terminal_controls() {
    let fx = build(&ctrl_fixture());
    let input = ".evil\u{1b}\u{202e}\n.param x \u{1b}\u{202e}\n.params\n[:find ?n :where [:ctrl :name ?n]]\n";
    let o = repl(&fx, input.as_bytes(), &[]);
    assert_eq!(code(&o), 1);
    for text in [stdout(&o), stderr(&o)] {
        no_raw_controls(&text);
        assert!(!text.contains('\u{202e}'), "{text:?}");
    }
    assert!(stdout(&o).contains(&fx.text("query", &["[:find ?n :where [:ctrl :name ?n]]"])));
}

#[test]
fn no_history_or_world_writes_happen_by_default() {
    let fx = varrow();
    let before = std::fs::read(&fx.path).unwrap();
    let files = dir_names(fx.dir.path());
    let o = repl(&fx, format!("{ALLIES}\n.quit\n").as_bytes(), &[]);
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    assert_eq!(std::fs::read(&fx.path).unwrap(), before);
    assert_eq!(dir_names(fx.dir.path()), files);
}

#[test]
fn cannot_open_initial_world_exits_three() {
    let dir = tempfile::tempdir().unwrap();
    let o = kala(["repl".as_ref(), dir.path().join("missing.kala").as_os_str()]);
    assert_eq!(code(&o), 3, "{}", stderr(&o));
    assert!(o.stdout.is_empty());
}

struct Live {
    child: std::process::Child,
    input: std::process::ChildStdin,
    lines: std::sync::mpsc::Receiver<String>,
}

impl Live {
    fn new(fx: &Fx) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_kala"))
            .arg("repl")
            .arg(&fx.path)
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

    fn query_result(&self) -> Json {
        loop {
            let line = self
                .lines
                .recv_timeout(std::time::Duration::from_secs(20))
                .expect("REPL must flush each result while stdin remains open");
            if let Ok(value) = serde_json::from_str::<Json>(&line)
                && value["command"] == "query"
            {
                return value;
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
fn snapshot_stays_pinned_and_writer_opens_until_reload() {
    use kala_core::{Tick, Value, World};
    let fx = varrow();
    let mut live = Live::new(&fx);
    let q = "[:find ?n :where [:edwin :name ?n]]\n";
    live.send(&format!(".json on\n{q}"));
    let before = live.query_result();
    assert_eq!(before["canon"], 12);
    // First result synchronizes with the child's completed read-only open.
    let mut writer = World::open(&fx.path).expect("REPL must not hold the writer's lock");
    let edwin = writer.entity_by_handle("edwin").unwrap();
    writer
        .assert(
            edwin,
            "name",
            Value::Str("new name".into()),
            Some(Tick(800)),
        )
        .unwrap();
    let latest = writer.latest_tx().0;
    drop(writer);
    live.send(q);
    assert_eq!(
        live.query_result(),
        before,
        "new commits do not mutate the snapshot"
    );
    live.send(&format!(".reload\n{q}"));
    let after = live.query_result();
    assert_eq!(after["canon"], latest);
    assert_eq!(after["tick"], 800);
    assert_ne!(after["rows"], before["rows"]);
}

#[test]
fn force_replacement_is_seen_only_after_reload() {
    let fx = varrow();
    let mut live = Live::new(&fx);
    let q = "[:find ?n :where [?e :name ?n]]\n";
    live.send(&format!(".json on\n{q}"));
    let before = live.query_result();
    let replacement = fx.dir.path().join("replacement.yaml");
    std::fs::write(&replacement, OTHER_FIXTURE).unwrap();
    let out = kala([
        "load".as_ref(),
        "--force".as_ref(),
        replacement.as_os_str(),
        fx.path.as_os_str(),
    ]);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    live.send(q);
    assert_eq!(live.query_result(), before);
    live.send(&format!(".reload\n{q}"));
    let after = live.query_result();
    assert_eq!(after["rows"].as_array().unwrap().len(), 1);
    assert_eq!(after["rows"][0][0]["value"], "Zed");
}
