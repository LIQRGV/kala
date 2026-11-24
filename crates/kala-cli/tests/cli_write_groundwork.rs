//! Slice 15a exercises language writes through the real CLI adapter.
mod common;
use common::*;
use kala_core::{Op, Precision, Tick, Value, World};
use serde_json::Value as Json;
use std::fs;
use std::io::Write;
use std::process::{Command, Output, Stdio};

const EMPTY: &str = r#"
schema:
  node_types: [CHARACTER, PLACE]
  attributes:
    - { name: located_in, type: ref }
    - { name: alias, type: string, cardinality: many }
    - { name: name, type: string }
    - { name: age, type: i64 }
    - { name: bought, type: ref, kind: event }
entities:
  CHARACTER: [greg, edwin]
  PLACE: [varrow, ashford, carnath]
"#;

fn transact(f: &Fx, text: &str, json: bool) -> Output {
    let input = f.dir.path().join("groundwork.edn");
    fs::write(&input, text).unwrap();
    let mut args = vec![input.to_str().unwrap()];
    if json {
        args.push("--json");
    }
    f.run("transact", &args)
}
fn apply(f: &Fx, text: &str) -> Json {
    let o = transact(f, text, true);
    assert_eq!(code(&o), 0, "{text}: {}", stderr(&o));
    parse_one_json(&stdout(&o))
}
fn values(f: &Fx, who: &str, attr: &str, tick: i64) -> Vec<Value> {
    let w = World::open_read_only(&f.path).unwrap();
    w.as_of(w.entity_by_handle(who).unwrap(), attr, Tick(tick))
        .unwrap()
}
fn strings(values: &[&str]) -> Vec<Value> {
    values.iter().map(|s| Value::Str((*s).into())).collect()
}

#[test]
fn if_missing_creates_then_reuses_handles_without_appending() {
    let f = build(EMPTY);
    let source =
        "[[:create :CHARACTER :kael2 :if-missing] [:assert :kael2 :name \"Kael\" :at 700]]";
    let first = apply(&f, source);
    assert_eq!(first["changed"], 1);
    assert_eq!(first["ops"][0]["created"], true);
    let before = fs::read(&f.path).unwrap();
    let dump = f.text("dump", &[]);
    let second = apply(&f, source);
    assert_eq!(second["changed"], 0);
    assert_eq!(second["declared"], 0);
    assert!(second["tx"].is_null());
    assert_eq!(second["ops"][0]["created"], false);
    assert_eq!(first["ops"][0]["entity"], second["ops"][0]["entity"]);
    assert_eq!(fs::read(&f.path).unwrap(), before);
    assert_eq!(f.text("dump", &[]), dump);
    let output = transact(&f, "[:create :CHARACTER :kael2 :if-missing]", false);
    assert_eq!(code(&output), 0, "{}", stderr(&output));
    assert!(stdout(&output).contains("nothing committed: 0 changed"));
    assert!(
        stdout(&output).contains("exists CHARACTER#3"),
        "{}",
        stdout(&output)
    );
    assert_eq!(fs::read(&f.path).unwrap(), before);
}

#[test]
fn if_missing_sees_staged_creates_and_reports_the_existing_type() {
    let f = build(EMPTY);
    let j = apply(
        &f,
        "[[:create :CHARACTER :kael2 :if-missing] [:create :CHARACTER :kael2 :if-missing] [:assert :kael2 :age 18 :at -1]]",
    );
    assert_eq!(j["ops"][0]["created"], true);
    assert_eq!(j["ops"][1]["created"], false);
    assert_eq!(j["ops"][0]["entity"], j["ops"][1]["entity"]);
    assert_eq!(j["changed"], 1);
    for (source, message) in [
        ("[:create :CHARACTER :varrow :if-missing]", "PLACE"),
        ("[:create :CHARACTER :kael2]", "already in use"),
    ] {
        let before = fs::read(&f.path).unwrap();
        let o = transact(&f, source, false);
        assert_eq!(code(&o), 1, "{}", stderr(&o));
        assert!(stderr(&o).contains(message), "{}", stderr(&o));
        assert_eq!(fs::read(&f.path).unwrap(), before);
    }
}

#[test]
fn if_missing_obeys_strict_node_types_even_for_existing_handles() {
    let f = build(EMPTY);
    apply(&f, "[:world-setting :schema :strict]");
    let before = fs::read(&f.path).unwrap();
    let o = transact(&f, "[:create :UNKNOWN :greg :if-missing]", false);
    assert_eq!(code(&o), 1, "{}", stderr(&o));
    assert!(stderr(&o).contains("UNKNOWN"), "{}", stderr(&o));
    assert_eq!(fs::read(&f.path).unwrap(), before);
}

#[test]
fn set_many_deduplicates_and_writes_only_the_difference() {
    let f = build(EMPTY);
    apply(
        &f,
        "[[:assert :edwin :alias \"a\" :at 0] [:assert :edwin :alias \"b\" :at 0]]",
    );
    let j = apply(
        &f,
        "[:set :edwin :alias [\"b\" \"c\" \"c\"] :at 10 :precision :year]",
    );
    assert_eq!(j["changed"], 2);
    assert_eq!(j["ops"].as_array().unwrap().len(), 1);
    assert_eq!(j["ops"][0]["kind"], "set");
    assert_eq!(j["ops"][0]["tick"], 10);
    assert_eq!(j["ops"][0]["precision"], "year");
    assert_eq!(j["ops"][0]["entries"].as_array().unwrap().len(), 2);
    assert!(j["warnings"].get("introduced_omitted").is_none());
    assert!(j["warnings"].get("resolved_omitted").is_none());
    assert_eq!(values(&f, "edwin", "alias", 9), strings(&["a", "b"]));
    assert_eq!(values(&f, "edwin", "alias", 10), strings(&["b", "c"]));
    let w = World::open_read_only(&f.path).unwrap();
    let h = w
        .history(w.entity_by_handle("edwin").unwrap(), "alias")
        .unwrap();
    let changes: Vec<_> = h.iter().filter(|c| c.tick == Tick(10)).collect();
    assert_eq!(changes.len(), 2);
    assert!(changes.iter().all(|c| c.precision == Precision::Year));
    assert!(
        changes
            .iter()
            .any(|c| c.op == Op::Retract && c.value == Value::Str("a".into()))
    );
    assert!(
        changes
            .iter()
            .any(|c| c.op == Op::Assert && c.value == Value::Str("c".into()))
    );
    drop(w);
    assert_eq!(apply(&f, "[:set :edwin :alias \"b\" :at 11]")["changed"], 1);
    assert_eq!(values(&f, "edwin", "alias", 11), strings(&["b"]));
    assert_eq!(apply(&f, "[:set :edwin :alias [] :at 12]")["changed"], 1);
    assert!(values(&f, "edwin", "alias", 12).is_empty());
    assert_eq!(apply(&f, "[:set :edwin :alias [] :at 12]")["changed"], 0);
}

#[test]
fn set_reads_at_its_own_tick_and_leaves_future_change_points() {
    let f = varrow();
    // Latest Greg location is Ashford; before tick150 it is Carnath.
    let greg = f.id("greg");
    let carnath = Value::Ref(f.id("carnath"));
    assert_eq!(
        values(&f, "greg", "located_in", 100),
        std::slice::from_ref(&carnath)
    );
    let j = apply(&f, "[:set :greg :located_in :varrow :at 100]");
    assert_eq!(j["changed"], 2);
    assert_eq!(values(&f, "greg", "located_in", 99), [carnath]);
    assert_eq!(
        values(&f, "greg", "located_in", 100),
        [Value::Ref(f.id("varrow"))]
    );
    assert!(values(&f, "greg", "located_in", 300).is_empty());
    assert_eq!(
        values(&f, "greg", "located_in", 601),
        [Value::Ref(f.id("ashford"))]
    );
    let w = World::open_read_only(&f.path).unwrap();
    let h = w.history(greg, "located_in").unwrap();
    assert_eq!(h.iter().filter(|c| c.tick == Tick(100)).count(), 2);
    assert!(h.iter().any(|c| c.tick == Tick(150) && c.op == Op::Retract));
}

#[test]
fn set_sees_earlier_ops_and_handles_edge_subjects() {
    let f = build(EMPTY);
    let j = apply(
        &f,
        "[[:create :CHARACTER :kael] [:assert :kael :name \"old\" :at -10] [:set :kael :name \"new\" :at -5] [:assert :kael :located_in :varrow :at -10] [:set [:kael :located_in :varrow] :disguised true :at -5]]",
    );
    assert_eq!(j["changed"], 5);
    assert_eq!(j["ops"][2]["changed"], 2);
    assert_eq!(values(&f, "kael", "name", -5), strings(&["new"]));
    let w = World::open_read_only(&f.path).unwrap();
    let edge = w
        .edge_id(
            w.entity_by_handle("kael").unwrap(),
            "located_in",
            w.entity_by_handle("varrow").unwrap(),
        )
        .unwrap();
    assert_eq!(
        w.as_of(edge.into(), "disguised", Tick(-5)).unwrap(),
        [Value::Bool(true)]
    );
}

#[test]
fn set_default_tick_is_the_transaction_start_tick_and_extremes_work() {
    let f = build(EMPTY);
    apply(&f, "[:assert :greg :name \"initial\" :at -100]");
    let j = apply(
        &f,
        "[[:assert :greg :age 1 :at 900] [:set :greg :name \"initial\"]]",
    );
    assert_eq!(j["ops"][1]["tick"], -100);
    assert_eq!(j["ops"][1]["changed"], 0);
    let j = apply(&f, "[:set :greg :name \"latest\"]");
    assert_eq!(j["ops"][0]["tick"], 900);
    assert_eq!(j["changed"], 2);
    let f = build(EMPTY);
    for (i, tick) in [i64::MIN, -1, 0, i64::MAX].into_iter().enumerate() {
        let j = apply(&f, &format!("[:set :greg :age {i} :at {tick}]"));
        assert_eq!(j["ops"][0]["tick"], tick);
        assert_eq!(values(&f, "greg", "age", tick), [Value::I64(i as i64)]);
        assert_eq!(j["changed"], if i == 0 { 1 } else { 2 });
    }
}

#[test]
fn set_refusals_are_atomic_and_same_tick_errors_offer_the_entry_correction() {
    let f = build(EMPTY);
    let first = apply(&f, "[:assert :greg :age 18 :at 601]");
    let entry = first["entries"][0].as_u64().unwrap();
    apply(&f, "[:deprecate :name]");
    for (source, message) in [
        (
            "[:set :greg :bought :varrow :at 700]",
            "events are never retracted",
        ),
        ("[:set :greg :name [] :at 700]", "deprecated"),
        ("[:set :greg :unknown [] :at 700]", "unknown"),
        ("[:set :greg :age \"bad\" :at 700]", "I64"),
        ("[:set :greg :age 19 :at 601]", "[:correct"),
    ] {
        let before = fs::read(&f.path).unwrap();
        let o = transact(&f, source, false);
        assert_eq!(code(&o), 1, "{source}: {}", stderr(&o));
        assert!(stderr(&o).contains(message), "{source}: {}", stderr(&o));
        if source.contains("19") {
            assert!(
                stderr(&o).contains(&format!("entry#{entry}")),
                "{}",
                stderr(&o)
            );
        }
        assert_eq!(fs::read(&f.path).unwrap(), before);
    }
}

#[test]
fn set_infers_nonempty_attributes_and_cardinality_one_is_only_a_warning() {
    let f = build(EMPTY);
    let j = apply(&f, "[:set :edwin :new_alias [\"a\" \"b\"] :at 0]");
    assert_eq!(j["declared"], 1);
    assert_eq!(j["changed"], 2);
    assert_eq!(j["warnings"]["introduced"].as_array().unwrap().len(), 1);
    assert_eq!(values(&f, "edwin", "new_alias", 0), strings(&["a", "b"]));
}

#[test]
fn set_and_if_missing_dump_only_primitives_and_replay_identically() {
    let f = varrow();
    apply(
        &f,
        "[[:create :CHARACTER :kael2 :if-missing] [:set :kael2 :alias [\"a\" \"b\"] :at 0]]",
    );
    apply(&f, "[:set :kael2 :alias [\"b\" \"c\"] :at 10]");
    let dump = f.text("dump", &[]);
    assert!(!dump.contains("[:set"));
    assert!(!dump.contains(":if-missing"));
    let input = f.dir.path().join("dump.edn");
    let replay = f.dir.path().join("replay.kala");
    fs::write(&input, &dump).unwrap();
    let output = kala([
        "transact",
        "--create",
        replay.to_str().unwrap(),
        input.to_str().unwrap(),
    ]);
    assert_eq!(code(&output), 0, "{}", stderr(&output));
    assert_eq!(fs::read(&f.path).unwrap(), fs::read(&replay).unwrap());
    let output = kala(["dump", replay.to_str().unwrap()]);
    assert_eq!(code(&output), 0, "{}", stderr(&output));
    assert_eq!(stdout(&output), dump);
}

#[test]
fn system_refuses_new_forms_in_transact_and_write_repl_without_touching_bytes() {
    let dir = tempfile::tempdir().unwrap();
    kala_system::system::ensure(dir.path()).unwrap();
    let path = dir.path().join("system.kala");
    let before = fs::read(&path).unwrap();
    for source in [
        "[:set :root :user \"changed\"]",
        "[:create :USER :attacker :if-missing]",
    ] {
        let input = dir.path().join("writes.edn");
        fs::write(&input, source).unwrap();
        let o = kala(["transact", path.to_str().unwrap(), input.to_str().unwrap()]);
        assert_eq!(code(&o), 1, "{}", stderr(&o));
        assert!(
            stderr(&o).contains("managed only by `kala user`"),
            "{}",
            stderr(&o)
        );
        let mut child = Command::new(env!("CARGO_BIN_EXE_kala"))
            .arg("repl")
            .arg(&path)
            .arg("--write")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        // The process may refuse before consuming input.
        let _ = child
            .stdin
            .take()
            .unwrap()
            .write_all(format!("{source}\n.quit\n").as_bytes());
        let o = child.wait_with_output().unwrap();
        assert_eq!(code(&o), 1, "{}", stderr(&o));
        assert!(
            stderr(&o).contains("managed only by `kala user`"),
            "{}",
            stderr(&o)
        );
        assert_eq!(fs::read(&path).unwrap(), before);
    }
}

#[test]
fn overlap_warning_cap_reports_exact_omitted_counts_and_doctor_keeps_total() {
    let f = build(EMPTY);
    let members = (0..17)
        .map(|n| format!("\"n{n:02}\""))
        .collect::<Vec<_>>()
        .join(" ");
    let source = format!("[:set :edwin :name [{members}] :at 0]");
    let j = apply(&f, &source);
    assert_eq!(j["warnings"]["introduced"].as_array().unwrap().len(), 100);
    assert_eq!(j["warnings"]["introduced_omitted"], 36);
    let doctor = f.json("doctor", &[]);
    assert_eq!(doctor["warnings"].as_array().unwrap().len(), 136);
    let output = transact(&f, "[:set :edwin :name [] :at 10]", false);
    assert_eq!(code(&output), 0, "{}", stderr(&output));
    assert!(
        stdout(&output).contains("36 more overlap warnings (run kala doctor to list them)"),
        "{}",
        stdout(&output)
    );
}

#[test]
fn set_expansion_limit_refuses_atomically_even_when_the_input_is_small() {
    let f = build(EMPTY);
    let mut w = World::open(&f.path).unwrap();
    let edwin = w.entity_by_handle("edwin").unwrap();
    let mut tx = w.begin();
    for i in 0..257 {
        tx.assert(edwin, "alias", Value::Str(format!("a{i}")), Some(Tick(0)))
            .unwrap();
    }
    tx.commit().unwrap();
    drop(w);
    let before = fs::read(&f.path).unwrap();
    let o = transact(&f, "[:set :edwin :alias [] :at 10]", false);
    assert_eq!(code(&o), 1, "{}", stderr(&o));
    assert!(stderr(&o).contains("limit"), "{}", stderr(&o));
    assert_eq!(fs::read(&f.path).unwrap(), before);
}

#[test]
fn set_same_tick_reassertion_names_the_conflicting_retract_entry() {
    let f = build(EMPTY);
    apply(&f, "[:assert :greg :age 18 :at 0]");
    let retraction = apply(&f, "[:retract :greg :age 18 :at 10]");
    let entry = retraction["entries"][0].as_u64().unwrap();
    let before = fs::read(&f.path).unwrap();
    let output = transact(&f, "[:set :greg :age 18 :at 10]", false);
    assert_eq!(code(&output), 1, "{}", stderr(&output));
    assert!(
        stderr(&output).contains(&format!("entry#{entry}")),
        "{}",
        stderr(&output)
    );
    assert!(stderr(&output).contains("[:correct"), "{}", stderr(&output));
    assert_eq!(fs::read(&f.path).unwrap(), before);
}

#[test]
fn set_on_bidirectional_relationship_reads_both_stored_orientations() {
    let f = build(EMPTY);
    apply(
        &f,
        "[[:attribute :ally :ref :many :bidirectional] [:create :CHARACTER :mara] [:assert :greg :ally :edwin :at 0]]",
    );
    // Greg's ID sorts before Edwin's; Edwin must read that reverse orientation.
    let j = apply(&f, "[:set :edwin :ally :mara :at 10]");
    assert_eq!(j["changed"], 2);
    assert!(values(&f, "greg", "ally", 10).is_empty());
    let w = World::open_read_only(&f.path).unwrap();
    let edwin = w.entity_by_handle("edwin").unwrap();
    let mara = w.entity_by_handle("mara").unwrap();
    assert_eq!(
        w.as_of(edwin, "ally", Tick(10)).unwrap(),
        [Value::Ref(mara)]
    );
    assert_eq!(
        w.as_of(mara, "ally", Tick(10)).unwrap(),
        [Value::Ref(edwin)]
    );
}

#[test]
fn set_reads_and_replaces_unindexed_long_strings_by_value() {
    let f = build(EMPTY);
    apply(&f, "[:attribute :bio :string :unindexed :many]");
    let a = "a".repeat(2048);
    let b = "b".repeat(2048);
    let c = "c".repeat(2048);
    apply(&f, &format!("[:set :edwin :bio [\"{a}\" \"{b}\"] :at 0]"));
    let result = apply(&f, &format!("[:set :edwin :bio [\"{b}\" \"{c}\"] :at 10]"));
    assert_eq!(result["changed"], 2);
    assert_eq!(values(&f, "edwin", "bio", 10), strings(&[&b, &c]));
    assert_eq!(
        apply(&f, &format!("[:set :edwin :bio [\"{b}\" \"{c}\"] :at 10]"))["changed"],
        0
    );
}

#[test]
fn set_expansions_share_one_form_budget_and_fail_atomically() {
    let f = build(EMPTY);
    let mut w = World::open(&f.path).unwrap();
    let edwin = w.entity_by_handle("edwin").unwrap();
    let greg = w.entity_by_handle("greg").unwrap();
    let mut tx = w.begin();
    for e in [edwin, greg] {
        for i in 0..129 {
            tx.assert(e, "alias", Value::Str(format!("a{i}")), Some(Tick(0)))
                .unwrap();
        }
    }
    tx.commit().unwrap();
    drop(w);
    let before = fs::read(&f.path).unwrap();
    let failed = transact(
        &f,
        "[[:set :edwin :alias [] :at 10] [:set :greg :alias [] :at 10]]",
        false,
    );
    assert_eq!(code(&failed), 1, "{}", stderr(&failed));
    assert!(stderr(&failed).contains("limit"), "{}", stderr(&failed));
    assert_eq!(fs::read(&f.path).unwrap(), before);
    let boundary = apply(
        &f,
        "[[:set :edwin :alias [] :at 10] [:set :greg :alias [\"a0\" \"a1\"] :at 10]]",
    );
    assert_eq!(boundary["changed"], 256);
}

#[test]
fn new_write_errors_escape_malicious_control_characters() {
    let f = build(EMPTY);
    // Quoted handles permit controls; a type mismatch forces the echoed handle
    // through the error path without imposing a new handle-validation rule.
    apply(&f, r#"[:create :PLACE "bad\n\u001b[31m"]"#);
    for source in [
        r#"[:set :greg "absent\n\u001b[31m" []]"#,
        r#"[:create :CHARACTER "bad\n\u001b[31m" :if-missing]"#,
    ] {
        let before = fs::read(&f.path).unwrap();
        let output = transact(&f, source, false);
        assert_eq!(code(&output), 1, "{}", stderr(&output));
        let error = stderr(&output);
        no_raw_controls(&error);
        assert_eq!(
            error.lines().count(),
            1,
            "input newline must not forge an error line: {error:?}"
        );
        assert_eq!(fs::read(&f.path).unwrap(), before);
    }
}

#[test]
fn existing_if_missing_handle_dumps_and_replays_byte_identically() {
    let f = build(EMPTY);
    let mut w = World::open(&f.path).unwrap();
    let id = w.create_entity("CHARACTER", Some("if-missing")).unwrap();
    w.assert(
        id,
        "name",
        Value::Str("Literal handle".into()),
        Some(Tick(-1)),
    )
    .unwrap();
    drop(w);
    let dump = f.text("dump", &[]);
    let input = f.dir.path().join("legacy-handle.edn");
    let replay = f.dir.path().join("legacy-replayed.kala");
    fs::write(&input, &dump).unwrap();
    let o = kala([
        "transact",
        "--create",
        replay.to_str().unwrap(),
        input.to_str().unwrap(),
    ]);
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    assert_eq!(fs::read(&f.path).unwrap(), fs::read(&replay).unwrap());
    let o = kala(["dump", replay.to_str().unwrap()]);
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    assert_eq!(stdout(&o), dump);
}
