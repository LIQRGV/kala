//! Schema changes through the real terminal binary, including reopened reads.
mod common;

use common::*;
use kala_core::{Tick, TxId, Value, World};
use serde_json::Value as Json;
use std::fs;
use std::io::Write;
use std::process::{Command, Output, Stdio};

fn loaded_varrow() -> Fx {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("world.kala");
    let output = kala(["load", VARROW, path.to_str().unwrap()]);
    assert_eq!(code(&output), 0, "{}", stderr(&output));
    let mem = kala_core::load_fixture(&fs::read_to_string(VARROW).unwrap()).unwrap();
    Fx { dir, path, mem }
}

fn transact(f: &Fx, source: &str, flags: &[&str]) -> Output {
    let input = f.dir.path().join("schema.edn");
    fs::write(&input, source).unwrap();
    let mut args = vec![input.to_str().unwrap()];
    args.extend_from_slice(flags);
    f.run("transact", &args)
}

fn write(f: &Fx, source: &str) -> String {
    let o = transact(f, source, &[]);
    assert_eq!(code(&o), 0, "{source}: {}", stderr(&o));
    stdout(&o)
}

fn json_write(f: &Fx, source: &str) -> Json {
    let o = transact(f, source, &["--json"]);
    assert_eq!(code(&o), 0, "{source}: {}", stderr(&o));
    serde_json::from_slice(&o.stdout).unwrap()
}

fn refuses_unchanged(f: &Fx, source: &str, fragments: &[&str]) {
    let before = fs::read(&f.path).unwrap();
    let o = transact(f, source, &[]);
    assert_eq!(code(&o), 1, "{source}: {}", stdout(&o));
    let error = stderr(&o);
    for fragment in fragments {
        assert!(error.contains(fragment), "missing {fragment:?}: {error}");
    }
    assert_eq!(fs::read(&f.path).unwrap(), before, "{source}");
}

fn no_integrity_errors(f: &Fx) {
    let report = f.json("doctor", &[]);
    assert!(report["errors"].as_array().unwrap().is_empty(), "{report}");
    World::open_read_only(&f.path).unwrap();
}

fn values(f: &Fx, entity: &str, attr: &str, tick: i64) -> Vec<Value> {
    let w = World::open_read_only(&f.path).unwrap();
    w.as_of(w.entity_by_handle(entity).unwrap(), attr, Tick(tick))
        .unwrap()
}

#[test]
fn unknown_attributes_and_node_types_are_inferred_and_reported() {
    let f = loaded_varrow();
    let report = write(&f, "[:assert :edwin :age 42 :at 0]");
    assert!(
        report.contains("declared :age as i64 (inferred)"),
        "{report}"
    );
    assert_eq!(values(&f, "edwin", "age", 0), [Value::I64(42)]);
    let shown = stdout(&f.run("asof", &["edwin", "age", "--at", "0"]));
    assert!(shown.contains("age = 42"), "{shown}");
    refuses_unchanged(
        &f,
        "[:assert :greg :age \"old\" :at 0]",
        &["age", "I64", "Str"],
    );

    let j = json_write(&f, "[:assert :edwin :mentor_of :tomas :at 0]");
    assert_eq!(j["schema"].as_array().unwrap().len(), 1, "{j}");
    assert_eq!(j["edges"].as_array().unwrap().len(), 1, "{j}");
    let w = World::open_read_only(&f.path).unwrap();
    let def = w.attribute("mentor_of").unwrap();
    assert_eq!(def.value_type, kala_core::ValueType::Ref);
    assert_eq!(def.kind, kala_core::Kind::State);
    assert_eq!(def.cardinality, kala_core::Cardinality::Many);
    assert_eq!(def.direction, Some(kala_core::RefDirection::Directed));
    drop(w);

    let report = write(&f, "[:create :FACTION :red_hand]");
    assert!(
        report.contains("declared node type FACTION (inferred)"),
        "{report}"
    );
    let w = World::open_read_only(&f.path).unwrap();
    assert_eq!(
        w.format_id(w.entity_by_handle("red_hand").unwrap()),
        "FACTION#1"
    );
    no_integrity_errors(&f);
}

#[test]
fn strict_setting_applies_in_operation_order_and_explicit_forms_work() {
    let f = loaded_varrow();
    write(&f, "[:world-setting :schema :strict]");
    refuses_unchanged(
        &f,
        "[:assert :edwin :height 180]",
        &["height", "[:attribute :height :i64]"],
    );
    refuses_unchanged(
        &f,
        "[:create :GUILD :g1]",
        &["GUILD", "[:node-type :GUILD]"],
    );
    write(
        &f,
        "[[:attribute :height :i64] [:assert :edwin :height 180] [:node-type :GUILD] [:create :GUILD :g1]]",
    );
    assert_eq!(values(&f, "edwin", "height", 700), [Value::I64(180)]);
    write(
        &f,
        "[[:world-setting :schema :infer] [:assert :greg :age 50 :at 10]]",
    );
    assert_eq!(values(&f, "greg", "age", 10), [Value::I64(50)]);
    refuses_unchanged(
        &f,
        "[[:world-setting :schema :strict] [:assert :edwin :unknown 1]]",
        &["unknown"],
    );
    // The failed setting was rolled back along with its write.
    write(&f, "[:assert :edwin :still_infers true]");
    no_integrity_errors(&f);
}

#[test]
fn schema_lists_origin_first_transaction_and_definitions_at_canon() {
    let f = loaded_varrow();
    let initial = f.json("schema", &[]);
    assert_eq!(
        initial["attributes"].as_array().unwrap().len(),
        11,
        "{initial}"
    );
    assert_eq!(
        initial["node_types"].as_array().unwrap().len(),
        4,
        "{initial}"
    );
    for def in initial["attributes"].as_array().unwrap() {
        assert_eq!(def["inferred"], false, "{def}");
        assert_eq!(def["first_tx"], 0, "{def}");
    }
    let j = json_write(
        &f,
        "[[:assert :edwin :age 42 :at 0] [:create :FACTION :red_hand]]",
    );
    let schema = f.json("schema", &[]);
    let age = schema["attributes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["name"] == "age")
        .unwrap();
    assert_eq!(age["inferred"], true);
    assert_eq!(age["first_tx"], j["tx"]);
    let faction = schema["node_types"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["name"] == "FACTION")
        .unwrap();
    assert_eq!(faction["inferred"], true);
    assert_eq!(faction["first_tx"], j["tx"]);
    let text = stdout(&f.run("schema", &[]));
    assert!(
        text.contains("age") && text.contains("inferred") && text.contains("declared"),
        "{text}"
    );
    write(&f, "[:attribute :age :i64]");
    let declared = f.json("schema", &[]);
    let age = declared["attributes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["name"] == "age")
        .unwrap();
    assert_eq!(age["inferred"], false);
    assert_eq!(age["first_tx"], j["tx"]);
    let before = f.json("schema", &["--canon", "tx#12"]);
    assert!(
        !before["attributes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|a| a["name"] == "age")
    );
    assert!(
        !before["node_types"]
            .as_array()
            .unwrap()
            .iter()
            .any(|a| a["name"] == "FACTION")
    );
    no_integrity_errors(&f);
}

#[test]
fn rename_uses_latest_names_at_every_canon_and_releases_old_name() {
    let f = loaded_varrow();
    let report = write(&f, "[:rename :located_in :location]");
    assert!(
        report.contains("renamed :located_in to :location"),
        "{report}"
    );
    for canon in ["", " :canon 12"] {
        let query = format!("[:find ?p :at 220{canon} :where [:greg :location ?p]]");
        let rows = f.json("query", &[&query]);
        assert_eq!(rows["rows"][0][0]["value"]["handle"], "varrow");
        let query = format!("[:find ?p :at 220{canon} :where [:greg :located_in ?p]]");
        let o = f.run("query", &[&query]);
        assert_eq!(code(&o), 1);
    }
    write(&f, "[:attribute :located_in :string]");
    let o = f.run(
        "query",
        &["[:find ?p :at 220 :canon 12 :where [:greg :located_in ?p]]"],
    );
    assert_eq!(code(&o), 1);
    let error = stderr(&o);
    assert!(
        error.contains("located_in") && error.contains("not defined") && error.contains("tx#12"),
        "{error}"
    );
    no_integrity_errors(&f);
}

#[test]
fn every_read_command_refuses_attributes_not_defined_at_its_canon() {
    let f = loaded_varrow();
    let mut world = World::open(&f.path).unwrap();
    let edwin = world.entity_by_handle("edwin").unwrap();
    let tomas = world.entity_by_handle("tomas").unwrap();
    let mut tx = world.begin();
    tx.infer_attribute("age", &Value::I64(42)).unwrap();
    tx.assert(edwin, "age", Value::I64(42), Some(Tick(0)))
        .unwrap();
    tx.infer_attribute("mentor_of", &Value::Ref(tomas)).unwrap();
    tx.assert(edwin, "mentor_of", Value::Ref(tomas), Some(Tick(0)))
        .unwrap();
    tx.commit().unwrap();
    drop(world);
    for (command, args) in [
        (
            "asof",
            vec!["edwin", "age", "--at", "0", "--canon", "tx#12"],
        ),
        ("history", vec!["edwin", "age", "--canon", "tx#12"]),
        ("find", vec!["age", "42", "--at", "0", "--canon", "tx#12"]),
        (
            "neighbors",
            vec!["edwin", "mentor_of", "--at", "0", "--canon", "tx#12"],
        ),
        (
            "edge",
            vec![
                "edwin",
                "mentor_of",
                "tomas",
                "--at",
                "0",
                "--canon",
                "tx#12",
            ],
        ),
    ] {
        let o = f.run(command, &args);
        assert_eq!(code(&o), 1, "{command}: {}", stdout(&o));
        let error = stderr(&o);
        assert!(
            error.contains("not defined") && error.contains("tx#12"),
            "{command}: {error}"
        );
    }
    no_integrity_errors(&f);
}

#[test]
fn cardinality_warnings_use_the_requested_schema_canon() {
    let f = loaded_varrow();
    write(&f, "[:cardinality :located_in :many]");
    let old = stdout(&f.run("doctor", &["--canon", "tx#8"]));
    for expected in ["greg", "located_in", "varrow", "ashford", "601"] {
        assert!(old.contains(expected), "missing {expected}: {old}");
    }
    let report = write(&f, "[:cardinality :ally_of :one]");
    for expected in ["warning", "edwin", "tomas", "greg", "-280"] {
        assert!(report.contains(expected), "missing {expected}: {report}");
    }
    no_integrity_errors(&f);
}

#[test]
fn deprecation_rejects_assert_retract_but_accepts_correction_and_preserves_old_reads() {
    let f = loaded_varrow();
    let w = World::open_read_only(&f.path).unwrap();
    let name_entry = w
        .entries()
        .find_map(|e| match &e.body {
            kala_core::log::EntryBody::Fact { attr, .. } if w.attr_name(*attr) == Some("name") => {
                Some(e.id.0)
            }
            _ => None,
        })
        .unwrap();
    drop(w);
    write(&f, "[:deprecate :name]");
    for source in [
        "[:assert :edwin :name \"X\"]".to_owned(),
        "[:retract :edwin :name \"Edwin\" :at -250]".to_owned(),
    ] {
        refuses_unchanged(&f, &source, &["name", "deprecated"]);
    }
    for canon in ["", " :canon 12"] {
        let query = format!("[:find ?n :at -250{canon} :where [:edwin :name ?n]]");
        assert_eq!(f.json("query", &[&query])["rows"][0][0]["value"], "Edwin");
    }
    write(&f, &format!("[:correct {name_entry} :delete]"));
    assert_eq!(
        f.json(
            "query",
            &["[:find ?n :at -250 :canon 12 :where [:edwin :name ?n]]"]
        )["rows"][0][0]["value"],
        "Edwin"
    );
    let report = write(&f, "[:deprecate :name]");
    assert!(report.contains("0 changed"), "{report}");
    no_integrity_errors(&f);
}

#[test]
fn scalar_migration_copies_history_reports_schema_and_preserves_old_canon() {
    let f = loaded_varrow();
    let added = json_write(
        &f,
        "[[:assert :edwin :age 42 :at 0] [:assert :greg :age 50 :at 10]]",
    );
    let canon = added["tx"].as_u64().unwrap();
    let report = write(&f, "[:migrate :age :string]");
    for expected in [
        "age@old",
        "deprecated",
        "declared",
        "2 change points copied",
        "0 skipped",
    ] {
        assert!(report.contains(expected), "missing {expected}: {report}");
    }
    assert_eq!(values(&f, "edwin", "age", 0), [Value::Str("42".into())]);
    assert_eq!(values(&f, "edwin", "age@old", 0), [Value::I64(42)]);
    let new = f.json("history", &["edwin", "age"]);
    let old = f.json("history", &["edwin", "age@old"]);
    for (new, old) in new["changes"]
        .as_array()
        .unwrap()
        .iter()
        .zip(old["changes"].as_array().unwrap())
    {
        assert_eq!(new["tick"], old["tick"]);
        assert_eq!(new["precision"], old["precision"]);
        assert_eq!(new["op"], old["op"]);
        assert_eq!(
            new["value"]["value"].as_str().unwrap(),
            old["value"]["value"].to_string()
        );
    }
    assert_eq!(
        new["changes"].as_array().unwrap().len(),
        old["changes"].as_array().unwrap().len()
    );
    let query = format!("[:find ?a :at 0 :canon {canon} :where [:edwin :age ?a]]");
    let o = f.run("query", &[&query]);
    assert_eq!(code(&o), 1);
    assert!(stderr(&o).contains("not defined"), "{}", stderr(&o));
    let query = format!("[:find ?a :at 0 :canon {canon} :where [:edwin :age@old ?a]]");
    assert_eq!(f.json("query", &[&query])["rows"][0][0]["value"], 42);
    refuses_unchanged(&f, "[:assert :edwin :age@old 43 :at 1]", &["deprecated"]);
    no_integrity_errors(&f);
}

#[test]
fn migration_refuses_bad_values_and_collisions_and_can_skip_them() {
    let f = loaded_varrow();
    write(
        &f,
        "[[:assert :edwin :age \"42\" :at 0] [:assert :greg :age \"x\" :at 10]]",
    );
    refuses_unchanged(&f, "[:migrate :age :i64]", &["1", "unconvertible", "x"]);
    let report = write(&f, "[:migrate :age :i64 :skip-unconvertible]");
    assert!(report.contains("1 skipped"), "{report}");
    assert_eq!(values(&f, "edwin", "age", 0), [Value::I64(42)]);
    assert!(values(&f, "greg", "age", 10).is_empty());
    write(
        &f,
        "[[:assert :edwin :codes \"42\" :at 0] [:assert :edwin :codes \"042\" :at 10]]",
    );
    refuses_unchanged(&f, "[:migrate :codes :i64]", &["2", "unconvertible"]);
    let report = write(&f, "[:migrate :codes :i64 :skip-unconvertible]");
    assert!(report.contains("2 skipped"), "{report}");
    assert!(values(&f, "edwin", "codes", 10).is_empty());
    no_integrity_errors(&f);
}

#[test]
fn migration_dry_run_has_same_schema_report_and_leaves_bytes_identical() {
    let f = loaded_varrow();
    write(&f, "[:assert :edwin :age 42 :at 0]");
    let before = fs::read(&f.path).unwrap();
    let o = transact(&f, "[:migrate :age :string]", &["--dry-run", "--json"]);
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    let preview: Json = serde_json::from_slice(&o.stdout).unwrap();
    assert_eq!(preview["dry_run"], true);
    assert_eq!(fs::read(&f.path).unwrap(), before);
    let applied = json_write(&f, "[:migrate :age :string]");
    assert_eq!(preview["schema"], applied["schema"]);
    assert_eq!(preview["changed"], applied["changed"]);
    no_integrity_errors(&f);
}

#[test]
fn reference_migrations_copy_edge_properties_and_both_mutual_directions() {
    let f = loaded_varrow();
    let old = f.json("edge", &["edwin", "present", "carnath", "--at", "-230"]);
    assert_eq!(old["properties"][0]["values"][0]["value"]["value"], true);
    let report = write(&f, "[:migrate :present :bidirectional]");
    assert!(report.contains("edge"), "{report}");
    let new = f.json("edge", &["edwin", "present", "carnath", "--at", "-230"]);
    assert_ne!(new["edge"], old["edge"]);
    assert_eq!(new["properties"], old["properties"]);
    assert_eq!(
        f.json("history", &[new["edge"].as_str().unwrap(), "disguised"])["changes"],
        f.json("history", &[old["edge"].as_str().unwrap(), "disguised"])["changes"]
    );
    for tick in ["-230", "-210"] {
        let new = f.json("edge", &["edwin", "present", "carnath", "--at", tick]);
        let old = f.json("edge", &["edwin", "present@old", "carnath", "--at", tick]);
        assert_eq!(new["properties"], old["properties"]);
    }
    write(&f, "[:migrate :present :directed]");
    for endpoints in [
        ["edwin", "present", "carnath"],
        ["carnath", "present", "edwin"],
    ] {
        let copied = f.json(
            "edge",
            &[endpoints[0], endpoints[1], endpoints[2], "--at", "-230"],
        );
        assert_eq!(copied["properties"], old["properties"]);
    }
    assert!(
        World::open_read_only(&f.path)
            .unwrap()
            .attribute("present@old2")
            .is_some()
    );
    write(&f, "[:migrate :ally_of :directed]");
    let w = World::open_read_only(&f.path).unwrap();
    let edwin = w.entity_by_handle("edwin").unwrap();
    let mara = w.entity_by_handle("mara").unwrap();
    assert!(
        w.as_of_value(edwin, "ally_of", &Value::Ref(mara), Tick(-208))
            .unwrap()
    );
    assert!(
        w.as_of_value(mara, "ally_of", &Value::Ref(edwin), Tick(-208))
            .unwrap()
    );
    assert_ne!(
        w.edge_id(edwin, "ally_of", mara),
        w.edge_id(mara, "ally_of", edwin)
    );
    drop(w);
    no_integrity_errors(&f);
}

#[test]
fn migration_refuses_opposed_directed_pairs_and_retracted_state_to_event() {
    let f = loaded_varrow();
    write(
        &f,
        "[[:assert :edwin :trusts :mara :at 0] [:assert :mara :trusts :edwin :at 0]]",
    );
    refuses_unchanged(&f, "[:migrate :trusts :bidirectional]", &["both"]);
    refuses_unchanged(&f, "[:migrate :located_in :event]", &["retract"]);
    write(&f, "[:migrate :born_in :state]");
    let w = World::open_read_only(&f.path).unwrap();
    assert_eq!(w.attribute("born_in").unwrap().kind, kala_core::Kind::State);
    assert!(
        !w.history(w.entity_by_handle("edwin").unwrap(), "born_in")
            .unwrap()
            .is_empty()
    );
    drop(w);
    no_integrity_errors(&f);
}

#[test]
fn reserved_names_and_failed_inference_never_leave_partial_schema() {
    let f = loaded_varrow();
    for source in [
        "[:attribute :age@old :i64]",
        "[:attribute :kala_handle :string]",
        "[:rename :name :kala_handle]",
        "[:rename :name :name@old]",
        "[:assert :edwin :oops@old 1]",
        "[[:assert :edwin :new_age 1] [:assert :greg :new_age \"old\"]]",
    ] {
        refuses_unchanged(&f, source, &[]);
    }
    let long = "x".repeat(257);
    refuses_unchanged(
        &f,
        &format!("[:assert :edwin :bio \"{long}\"]"),
        &["256", "[:attribute :bio :string :unindexed]"],
    );
    write(
        &f,
        &format!("[[:attribute :bio :string :unindexed] [:assert :edwin :bio \"{long}\"]]"),
    );
    assert_eq!(values(&f, "edwin", "bio", 700), [Value::Str(long)]);
    no_integrity_errors(&f);
}

fn repl(path: &std::path::Path, source: &str) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_kala"))
        .arg("repl")
        .arg(path)
        .arg("--write")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(source.as_bytes())
        .unwrap();
    child.wait_with_output().unwrap()
}

#[test]
fn repl_write_reports_schema_and_reads_new_attribute() {
    let f = loaded_varrow();
    let o = repl(
        &f.path,
        "[:assert :edwin :age 42 :at 0]\n[:find ?a :at 0 :where [:edwin :age ?a]]\n.quit\n",
    );
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    let text = stdout(&o);
    assert!(text.contains("declared :age as i64 (inferred)"), "{text}");
    assert!(text.contains("42"), "{text}");
    no_integrity_errors(&f);
}

#[test]
fn schema_forms_cannot_write_the_system_world_even_after_rename() {
    let dir = tempfile::tempdir().unwrap();
    kala_bolt::system::add(dir.path(), "author@localhost", None).unwrap();
    let path = dir.path().join("ordinary.kala");
    fs::rename(dir.path().join("system.kala"), &path).unwrap();
    let before = fs::read(&path).unwrap();
    let input = dir.path().join("schema.edn");
    fs::write(&input, "[:attribute :public :string]").unwrap();
    let o = kala(["transact", path.to_str().unwrap(), input.to_str().unwrap()]);
    assert_eq!(code(&o), 1);
    assert!(stderr(&o).contains("system"), "{}", stderr(&o));
    let o = repl(&path, "[:world-setting :schema :infer]\n");
    assert_eq!(code(&o), 1);
    assert!(stderr(&o).contains("system"), "{}", stderr(&o));
    assert_eq!(fs::read(&path).unwrap(), before);
}

#[test]
fn format_four_is_refused_with_recreation_guidance() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("old.kala");
    let mut bytes = kala_core::log::LOG_MAGIC.to_vec();
    bytes.extend(4_u32.to_le_bytes());
    fs::write(&path, &bytes).unwrap();
    let o = kala(["info", path.to_str().unwrap()]);
    assert_eq!(code(&o), 3); // UnsupportedVersion is a store error (slice 5).
    let message = stderr(&o);
    assert!(
        message.contains("older Kala") && message.contains("recreat"),
        "{message}"
    );
    assert_eq!(fs::read(path).unwrap(), bytes);
}

#[test]
fn migration_retains_latest_corrections_and_old_canon_trail() {
    let f = loaded_varrow();
    let first = json_write(&f, "[:assert :edwin :age 42 :at -200 :precision :year]");
    let id = first["entries"][0].as_u64().unwrap();
    let old_canon = first["tx"].as_u64().unwrap();
    write(&f, &format!("[:correct {id} :replace :value 43]"));
    // The later-written prequel creates a second assert of the same value.
    // Copying these in tick order through normal writes would lose -200.
    write(&f, "[:assert :edwin :age 43 :at -300]");
    write(&f, "[:migrate :age :string]");
    let w = World::open_read_only(&f.path).unwrap();
    let e = w.entity_by_handle("edwin").unwrap();
    let history = w.history(e, "age").unwrap();
    assert_eq!(history.len(), 2);
    assert_eq!(history[0].tick, Tick(-300));
    assert_eq!(history[0].value, Value::Str("43".into()));
    assert_eq!(history[1].tick, Tick(-200));
    assert_eq!(history[1].value, Value::Str("43".into()));
    assert_eq!(history[1].precision, kala_core::Precision::Year);
    assert_eq!(
        w.at_canon(TxId(old_canon))
            .as_of(e, "age@old", Tick(-200))
            .unwrap(),
        [Value::I64(42)]
    );
    no_integrity_errors(&f);
}
