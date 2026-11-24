//! Terminal writes exercise the real binary and the native store's rules.

mod common;
#[path = "common/damage.rs"]
mod damage;
use common::*;
use kala_core::{Tick, TxId, Value, World};
use serde_json::Value as Json;
use std::fs;
use std::process::Output;

const EMPTY: &str = r#"
schema:
  node_types: [CHARACTER, PLACE]
  attributes:
    - { name: located_in, type: ref }
    - { name: name, type: string }
    - { name: disguised, type: bool }
    - { name: age, type: i64 }
    - { name: height, type: f64 }
    - { name: bought, type: ref, kind: event }
entities:
  CHARACTER: [greg, edwin]
  PLACE: [varrow, ashford, carnath]
"#;

fn transact(f: &Fx, text: &str, options: &[&str]) -> Output {
    let input = f.dir.path().join("writes.edn");
    fs::write(&input, text).unwrap();
    let mut args = vec![input.to_str().unwrap()];
    args.extend_from_slice(options);
    f.run("transact", &args)
}

fn ok(f: &Fx, text: &str) -> String {
    let o = transact(f, text, &[]);
    assert_eq!(code(&o), 0, "write {text}: {}", stderr(&o));
    stdout(&o)
}

fn locations(f: &Fx, tick: i64) -> Vec<String> {
    let q = format!("[:find ?p :at {tick} :where [:greg :located_in ?p]]");
    let j = f.json("query", &[&q]);
    j["rows"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r[0]["value"]["handle"].as_str().unwrap().to_owned())
        .collect()
}

#[test]
fn set_moves_greg_without_an_overlap_warning() {
    let f = varrow();
    // Verify the fixture through the real binary before pinning the replacement.
    assert_eq!(locations(&f, 224), ["varrow"]);
    let output = ok(&f, "[:set :greg :located_in :ashford :at 225]");
    assert!(output.contains("2 changed"), "{output}");
    assert!(warning_lines(&output).is_empty(), "{output}");
    assert_eq!(locations(&f, 224), ["varrow"]);
    assert_eq!(locations(&f, 225), ["ashford"]);
    let before = fs::read(&f.path).unwrap();
    let output = ok(&f, "[:set :greg :located_in :ashford :at 225]");
    assert!(output.contains("nothing committed: 0 changed"), "{output}");
    assert_eq!(fs::read(&f.path).unwrap(), before);
}

#[test]
fn greg_overlap_warning_appears_and_resolves() {
    let f = build(EMPTY);
    let first = ok(&f, "[:assert :greg :located_in :varrow :at 200]");
    assert!(first.contains("1 changed"), "{first}");
    let second = ok(&f, "[:assert :greg :located_in :ashford :at 601]");
    assert!(
        second.contains("warning") && second.contains("greg") && second.contains("601"),
        "{second}"
    );
    assert!(
        second.contains("varrow") && second.contains("ashford"),
        "{second}"
    );
    let third = ok(&f, "[:retract :greg :located_in :varrow :at 250]");
    assert!(
        third.contains("resolved") && third.contains("greg"),
        "{third}"
    );
    assert_eq!(locations(&f, 220), ["varrow"]);
    // States end explicitly: there is a gap between departure and arrival.
    assert!(locations(&f, 300).is_empty());
    assert_eq!(locations(&f, 601), ["ashford"]);
}

fn documents(o: &Output) -> Vec<Json> {
    serde_json::Deserializer::from_slice(&o.stdout)
        .into_iter()
        .map(Result::unwrap)
        .collect()
}

fn json_write(f: &Fx, source: &str) -> Json {
    let o = transact(f, source, &["--json"]);
    assert_eq!(code(&o), 0, "{source}: {}", stderr(&o));
    let docs = documents(&o);
    assert_eq!(docs.len(), 1);
    docs.into_iter().next().unwrap()
}

#[test]
fn third_operation_error_aborts_entities_and_facts() {
    let f = build(EMPTY);
    let before = fs::read(&f.path).unwrap();
    let o = transact(
        &f,
        "[[:create :CHARACTER :kael] [:assert :kael :age 18 :at -300] [:retract :kael :age 18 :at -300]]",
        &[],
    );
    assert_eq!(code(&o), 1, "{}", stderr(&o));
    let err = stderr(&o);
    assert!(
        err.contains("op 3") && err.contains(":retract") && err.contains("opposite op"),
        "{err}"
    );
    assert_eq!(fs::read(&f.path).unwrap(), before);
    let w = World::open_read_only(&f.path).unwrap();
    assert!(w.entity_by_handle("kael").is_none());
    assert_eq!(w.latest_tx(), f.mem.latest_tx());
}

#[test]
fn forms_commit_in_order_and_stop_before_any_later_form() {
    let f = build(EMPTY);
    let o = transact(
        &f,
        "[:assert :greg :age 18 :at -300]\n[:assert :nobody :age 20]\n[:assert :greg :name \"too late\"]",
        &["--json"],
    );
    assert_eq!(code(&o), 1, "{}", stderr(&o));
    let docs = documents(&o);
    assert_eq!(docs.len(), 1, "{}", stdout(&o));
    assert_eq!(docs[0]["form"], 1);
    assert_eq!(docs[0]["changed"], 1);
    assert!(stderr(&o).contains("2"), "{}", stderr(&o));
    let w = World::open_read_only(&f.path).unwrap();
    let greg = w.entity_by_handle("greg").unwrap();
    assert_eq!(w.as_of(greg, "age", Tick(0)).unwrap(), [Value::I64(18)]);
    assert!(w.as_of(greg, "name", Tick(0)).unwrap().is_empty());
}

#[test]
fn malformed_later_form_keeps_earlier_commits() {
    let f = build(EMPTY);
    let o = transact(
        &f,
        "[:assert :greg :age 18 :at -300]\n[:assert :greg]\n[:assert :greg :name \"too late\"]",
        &["--json"],
    );
    assert_eq!(code(&o), 1, "{}", stderr(&o));
    let docs = documents(&o);
    assert_eq!(docs.len(), 1, "{}", stdout(&o));
    assert_eq!(docs[0]["form"], 1);
    assert!(stderr(&o).contains("form 2"), "{}", stderr(&o));
    let w = World::open_read_only(&f.path).unwrap();
    assert_eq!(
        w.as_of(f.id("greg"), "age", Tick(0)).unwrap(),
        [Value::I64(18)]
    );
    assert!(w.as_of(f.id("greg"), "name", Tick(0)).unwrap().is_empty());
}

#[test]
fn json_reports_ticks_entries_new_edges_and_noops() {
    let f = build(EMPTY);
    let j = json_write(
        &f,
        "[:at -300 [:assert :greg :located_in :varrow :precision :year] [:assert :greg :located_in :varrow] [:assert :greg :age 18 :at -280]]",
    );
    assert!(j["tx"].is_u64());
    assert_eq!(j["changed"], 2);
    assert_eq!(j["dry_run"], false);
    assert_eq!(j["ops"].as_array().unwrap().len(), 3);
    assert_eq!(j["ops"][0]["position"], 1);
    assert_eq!(j["ops"][0]["tick"], -300);
    assert_eq!(j["ops"][0]["precision"], "year");
    assert_eq!(j["ops"][0]["changed"], 1);
    assert_eq!(j["ops"][0]["entries"].as_array().unwrap().len(), 1);
    assert_eq!(j["ops"][1]["changed"], 0);
    assert!(j["ops"][1]["entries"].as_array().unwrap().is_empty());
    assert_eq!(j["ops"][2]["tick"], -280);
    assert_eq!(j["edges"].as_array().unwrap().len(), 1);
    assert!(j["edges"][0].as_str().unwrap().starts_with("edge#"));
    assert_eq!(j["entries"].as_array().unwrap().len(), 2);
    let no = ok(&f, "[:assert :greg :age 18 :at -280]");
    assert!(no.contains("0 changed"), "{no}");
}

#[test]
fn entry_id_corrects_retract_and_old_canon_keeps_original_tick() {
    let f = build(EMPTY);
    let initial = json_write(
        &f,
        "[[:assert :greg :located_in :varrow :at -300] [:retract :greg :located_in :varrow :at -250]]",
    );
    let entry = initial["ops"][1]["entries"][0].as_u64().unwrap();
    let canon = initial["tx"].as_u64().unwrap();
    let corrected = json_write(&f, &format!("[:correct entry#{entry} :replace :at -280]"));
    assert_eq!(corrected["ops"][0]["tick"], -280);
    let w = World::open_read_only(&f.path).unwrap();
    let greg = w.entity_by_handle("greg").unwrap();
    let varrow = Value::Ref(w.entity_by_handle("varrow").unwrap());
    assert!(
        w.at_canon(TxId(canon))
            .as_of_value(greg, "located_in", &varrow, Tick(-270))
            .unwrap()
    );
    assert!(
        !w.as_of_value(greg, "located_in", &varrow, Tick(-270))
            .unwrap()
    );
    assert_eq!(
        w.history(greg, "located_in")
            .unwrap()
            .iter()
            .map(|c| c.tick.0)
            .collect::<Vec<_>>(),
        [-300, -280]
    );
}

#[test]
fn correction_delete_and_value_op_precision_replacements() {
    let f = build(EMPTY);
    let j = json_write(&f, "[:assert :greg :age 18 :at -300]");
    let id = j["ops"][0]["entries"][0].as_u64().unwrap();
    json_write(
        &f,
        &format!("[:correct {id} :replace :value 19 :precision :decade :op :assert]"),
    );
    let h = f.json("history", &["greg", "age"]);
    assert_eq!(h["changes"][0]["value"]["value"], 19);
    assert_eq!(h["changes"][0]["precision"], "decade");
    json_write(&f, &format!("[:correct {id} :delete]"));
    assert!(
        f.json("history", &["greg", "age"])["changes"]
            .as_array()
            .unwrap()
            .is_empty()
    );
}

#[test]
fn correction_noop_and_delete_report_current_precision() {
    let f = build(EMPTY);
    let j = json_write(&f, "[:assert :greg :age 18 :at -300 :precision :year]");
    let entry = j["ops"][0]["entries"][0].as_u64().unwrap();
    let no_op = json_write(&f, &format!("[:correct {entry} :replace]"));
    assert_eq!(no_op["changed"], 0);
    assert_eq!(no_op["ops"][0]["tick"], -300);
    assert_eq!(no_op["ops"][0]["precision"], "year");
    json_write(
        &f,
        &format!("[:correct {entry} :replace :at -280 :precision :decade]"),
    );
    let no_op = json_write(&f, &format!("[:correct {entry} :replace]"));
    assert_eq!(no_op["ops"][0]["tick"], -280);
    assert_eq!(no_op["ops"][0]["precision"], "decade");
    let deleted = json_write(&f, &format!("[:correct {entry} :delete]"));
    assert_eq!(deleted["ops"][0]["tick"], -280);
    assert_eq!(deleted["ops"][0]["precision"], "decade");
}

#[test]
fn created_handle_and_new_edge_resolve_inside_one_transaction() {
    let f = build(EMPTY);
    let j = json_write(
        &f,
        "[:at -300 [:create :CHARACTER :kael] [:assert :kael :located_in :carnath] [:assert [:kael :located_in :carnath] :disguised true]]",
    );
    assert_eq!(j["ops"].as_array().unwrap().len(), 3);
    let w = World::open_read_only(&f.path).unwrap();
    let kael = w.entity_by_handle("kael").unwrap();
    let carnath = w.entity_by_handle("carnath").unwrap();
    let edge = w.edge_id(kael, "located_in", carnath).unwrap();
    assert_eq!(
        w.as_of(edge.into(), "disguised", Tick(-300)).unwrap(),
        [Value::Bool(true)]
    );
    drop(w);
    let o = transact(&f, "[:create :CHARACTER :kael]", &[]);
    assert_eq!(code(&o), 1);
    assert!(
        stderr(&o).contains("op 1") && stderr(&o).contains(":create"),
        "{}",
        stderr(&o)
    );
}

#[test]
fn absent_edge_is_refused_and_existing_edge_properties_work() {
    let f = build(EMPTY);
    let o = transact(
        &f,
        "[:assert [:edwin :located_in :carnath] :disguised true :at -300]",
        &[],
    );
    assert_eq!(code(&o), 1, "{}", stderr(&o));
    assert!(stderr(&o).contains("edge"), "{}", stderr(&o));
    ok(&f, "[:assert :edwin :located_in :carnath :at -300]");
    ok(
        &f,
        "[:assert [:edwin :located_in :carnath] :disguised true :at -300]",
    );
    assert_eq!(
        f.json("edge", &["edwin", "located_in", "carnath", "@-300"])["properties"][0]["values"][0]
            ["value"]["value"],
        true
    );
}

#[test]
fn store_rejects_same_tick_event_retraction_and_wrong_value_type() {
    let f = build(EMPTY);
    ok(
        &f,
        "[[:assert :greg :age 18 :at -300] [:assert :greg :bought :varrow :at -300]]",
    );
    for (text, kind) in [
        ("[:retract :greg :age 18 :at -300]", ":retract"),
        ("[:retract :greg :bought :varrow :at -280]", ":retract"),
        ("[:assert :greg :age \"eighteen\" :at -280]", ":assert"),
    ] {
        let before = fs::read(&f.path).unwrap();
        let o = transact(&f, text, &[]);
        assert_eq!(code(&o), 1, "{text}: {}", stderr(&o));
        assert!(
            stderr(&o).contains("op 1") && stderr(&o).contains(kind),
            "{}",
            stderr(&o)
        );
        assert_eq!(fs::read(&f.path).unwrap(), before);
    }
}

#[test]
fn edge_ticks_prequel_and_transaction_start_default() {
    let f = build(EMPTY);
    for t in [i64::MIN, -300, 0, i64::MAX] {
        let j = json_write(&f, &format!("[:assert :greg :age {t} :at {t}]"));
        assert_eq!(j["ops"][0]["tick"], t);
        assert_eq!(j["changed"], 1);
    }
    let f = build(EMPTY);
    let j = json_write(
        &f,
        "[[:assert :greg :age 18 :at 100] [:assert :greg :name \"Greg\"]]",
    );
    assert_eq!(
        j["ops"][1]["tick"], 0,
        "same-transaction future write cannot move default"
    );
    assert_eq!(
        json_write(&f, "[:assert :greg :height 1.5]")["ops"][0]["tick"],
        100
    );
    assert_eq!(
        json_write(&f, "[:at -400 [:assert :greg :name \"Young Greg\"]]")["ops"][0]["tick"],
        -400
    );
}

#[test]
fn dry_run_reports_warnings_and_never_changes_log_or_entities() {
    let f = build(EMPTY);
    let before = fs::read(&f.path).unwrap();
    let o = transact(
        &f,
        "[[:create :CHARACTER :kael] [:assert :greg :located_in :varrow :at 200] [:assert :greg :located_in :ashford :at 601]]",
        &["--dry-run", "--json"],
    );
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    let docs = documents(&o);
    assert_eq!(docs.len(), 1);
    assert_eq!(docs[0]["dry_run"], true);
    assert_eq!(
        docs[0]["warnings"]["introduced"].as_array().unwrap().len(),
        1
    );
    assert_eq!(fs::read(&f.path).unwrap(), before);
    assert!(
        World::open_read_only(&f.path)
            .unwrap()
            .entity_by_handle("kael")
            .is_none()
    );
}

#[test]
fn dry_run_failure_also_leaves_log_identical() {
    let f = build(EMPTY);
    let before = fs::read(&f.path).unwrap();
    let o = transact(
        &f,
        "[[:create :CHARACTER :kael] [:assert :kael :age \"wrong\"]]",
        &["--dry-run"],
    );
    assert_eq!(code(&o), 1);
    assert_eq!(fs::read(&f.path).unwrap(), before);
}

#[test]
fn dry_run_never_recovers_or_quarantines_a_torn_log() {
    let f = build(EMPTY);
    ok(&f, "[:assert :greg :age 18 :at -300]");
    let mut bytes = fs::read(&f.path).unwrap();
    let last = damage::spans(&bytes).pop().unwrap();
    bytes.truncate(last.start + damage::BEGIN_LEN + 1);
    fs::write(&f.path, &bytes).unwrap();
    let source = f.dir.path().join("writes.edn");
    fs::write(&source, "[:assert :greg :age 20 :at -280]").unwrap();
    let before = damage::dir_snapshot(f.dir.path());
    let o = f.run(
        "transact",
        &[source.to_str().unwrap(), "--dry-run", "--json"],
    );
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    assert_eq!(documents(&o)[0]["dry_run"], true);
    assert_eq!(
        damage::dir_snapshot(f.dir.path()),
        before,
        "preview cannot recover the log or create quarantine files"
    );
}

#[test]
fn dry_run_refuses_damaged_tail_needing_acknowledgement_without_touching_it() {
    let f = build(EMPTY);
    ok(&f, "[:assert :greg :age 18 :at -300]");
    let mut bytes = fs::read(&f.path).unwrap();
    let last = damage::spans(&bytes).pop().unwrap();
    bytes[last.body_start + 6] ^= 0x01;
    fs::write(&f.path, bytes).unwrap();
    let source = f.dir.path().join("writes.edn");
    fs::write(&source, "[:assert :greg :age 20 :at -280]").unwrap();
    let before = damage::dir_snapshot(f.dir.path());
    let o = f.run(
        "transact",
        &[source.to_str().unwrap(), "--dry-run", "--json"],
    );
    assert!(
        !o.status.success(),
        "preview must refuse a write that needs recovery acknowledgement: {}",
        stdout(&o)
    );
    assert!(stderr(&o).contains("acknowledg"), "{}", stderr(&o));
    assert_eq!(damage::dir_snapshot(f.dir.path()), before);
}

#[test]
fn held_write_lock_is_exit_three() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("w.kala");
    // Retain the fixture writer: no unlock/reopen window while other test
    // threads spawn children, briefly inheriting a file descriptor at fork.
    let _held = kala_fixture::load_fixture_to(EMPTY, &path).unwrap();
    let f = Fx {
        dir,
        path,
        mem: kala_fixture::load_fixture(EMPTY).unwrap(),
    };
    let o = transact(&f, "[:assert :greg :age 18]", &[]);
    assert_eq!(code(&o), 3, "{}", stderr(&o));
    assert!(stderr(&o).to_lowercase().contains("lock"), "{}", stderr(&o));
}

#[test]
fn stdin_accepts_multiple_transactions_and_json_documents() {
    use std::io::Write;
    use std::process::{Command, Stdio};
    let f = build(EMPTY);
    let mut child = Command::new(env!("CARGO_BIN_EXE_kala"))
        .arg("transact")
        .arg(&f.path)
        .args(["-", "--json"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"[:assert :greg :age 18 :at 0]\n[:assert :greg :name \"Greg\" :at 1]\n")
        .unwrap();
    let o = child.wait_with_output().unwrap();
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    let docs = documents(&o);
    assert_eq!(docs.len(), 2);
    assert_eq!(docs[1]["form"], 2);
    assert_eq!(
        docs[1]["tx"].as_u64().unwrap(),
        docs[0]["tx"].as_u64().unwrap() + 1
    );
}

#[test]
fn input_limit_and_errors_escape_controls() {
    let f = build(EMPTY);
    let before = fs::read(&f.path).unwrap();
    let o = transact(&f, &" ".repeat(kala_datalog::MAX_QUERY_BYTES + 1), &[]);
    assert_eq!(code(&o), 1);
    assert_eq!(fs::read(&f.path).unwrap(), before);
    let o = transact(
        &f,
        "[:assert :greg :age \"\\u001b[31m\\u009b\\u202e\"]",
        &[],
    );
    assert_eq!(code(&o), 1);
    no_raw_controls(&stdout(&o));
    no_raw_controls(&stderr(&o));
    assert!(!stderr(&o).contains('\u{202e}'));
}

// ---- PR #3 review round 1 ----

fn damaged(f: &Fx) {
    ok(f, "[:assert :greg :age 18 :at -300]");
    let mut bytes = fs::read(&f.path).unwrap();
    let last = damage::spans(&bytes).pop().unwrap();
    bytes[last.body_start + 6] ^= 0x01;
    fs::write(&f.path, bytes).unwrap();
}

// A damaged last transaction is never cut by a CLI write without an explicit
// acknowledgement; retrying must not get past it.
#[test]
fn damaged_tail_is_refused_until_acknowledged_and_retry_does_not_bypass() {
    let f = build(EMPTY);
    damaged(&f);
    let before = damage::dir_snapshot(f.dir.path());
    let w = "[:assert :greg :age 20 :at -280]";
    for _ in 0..2 {
        let o = transact(&f, w, &[]);
        assert_eq!(code(&o), 3, "{}", stdout(&o));
        assert!(
            stderr(&o).contains("--acknowledge-recovery"),
            "{}",
            stderr(&o)
        );
        // The input file is rewritten by `transact`; compare everything else.
        let mut now = damage::dir_snapshot(f.dir.path());
        now.retain(|(name, _)| name != "writes.edn");
        let mut was = before.clone();
        was.retain(|(name, _)| name != "writes.edn");
        assert_eq!(now, was, "a refused write must not cut or quarantine");
    }
    let o = transact(&f, w, &["--acknowledge-recovery"]);
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    assert!(stdout(&o).contains("committed"), "{}", stdout(&o));
}

#[test]
fn repl_write_mode_refuses_a_damaged_tail_without_acknowledgement() {
    let f = build(EMPTY);
    damaged(&f);
    let len = fs::metadata(&f.path).unwrap().len();
    let o = f.run("repl", &["--write"]);
    assert_eq!(code(&o), 3, "{}", stderr(&o));
    assert!(
        stderr(&o).contains("--acknowledge-recovery"),
        "{}",
        stderr(&o)
    );
    assert_eq!(fs::metadata(&f.path).unwrap().len(), len);
}

// Dry-run previews build on each other like real commits.
#[test]
fn dry_run_forms_see_earlier_forms() {
    let f = build(EMPTY);
    let before = fs::read(&f.path).unwrap();
    let o = transact(
        &f,
        "[:at 800 [:create :CHARACTER :nerys]]\n[:assert :nerys :name \"Nerys\"]",
        &["--dry-run"],
    );
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    assert_eq!(fs::read(&f.path).unwrap(), before);
}

// A transaction that changes nothing commits nothing and says so.
#[test]
fn a_no_op_transaction_reports_nothing_committed() {
    let f = build(EMPTY);
    ok(&f, "[:assert :greg :age 18 :at -300]");
    let out = ok(&f, "[:assert :greg :age 18 :at -300]");
    assert!(out.contains("nothing committed"), "{out}");
    assert!(!out.contains("committed tx#"), "{out}");
    let o = transact(&f, "[:assert :greg :age 18 :at -300]", &["--json"]);
    let j: Json = serde_json::from_str(stdout(&o).lines().next().unwrap()).unwrap();
    assert!(j["tx"].is_null(), "{j}");
}

// A no-op doesn't claim a precision it didn't store.
#[test]
fn a_no_op_does_not_report_the_requested_precision() {
    let f = build(EMPTY);
    ok(&f, "[:assert :greg :age 18 :at -300]");
    let out = ok(&f, "[:assert :greg :age 18 :at -300 :precision :year]");
    assert!(!out.contains("(year)"), "{out}");
}
