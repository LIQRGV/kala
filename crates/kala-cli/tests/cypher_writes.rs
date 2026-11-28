mod common;
#[path = "../../kala-cypher/tests/common/write_cases.rs"]
mod write_cases;

use common::{code, kala, stderr, stdout};
use serde_json::Value;
use std::fs;
use std::path::Path;

fn transact(world: &Path, input: &Path, extra: &[&str]) -> std::process::Output {
    let mut args = vec![
        "transact",
        "--lang",
        "cypher",
        world.to_str().unwrap(),
        input.to_str().unwrap(),
    ];
    args.extend_from_slice(extra);
    kala(args)
}

#[test]
fn cypher_transact_splits_statements_honors_dry_run_and_stops_at_failure() {
    let fx = common::build(include_str!("../../kala-core/tests/fixtures/varrow.yaml"));
    let path = fx.dir.path().join("import.cypher");
    // The semicolon in the name belongs to the string, even in file mode.
    fs::write(
        &path,
        "CREATE (:CHARACTER {kala_handle:'unseen',name:'Semi;colon'}) AT TICK 800;",
    )
    .unwrap();
    let before = fs::read(&fx.path).unwrap();
    let o = transact(
        &fx.path,
        &path,
        &[
            "--json",
            "--dry-run",
            "--concurrency",
            "weak",
            "--busy-timeout",
            "2s",
        ],
    );
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    let report: Value = serde_json::from_slice(&o.stdout).unwrap();
    assert_eq!(report["changed"], 1);
    assert_eq!(report["dry_run"], true);
    assert_eq!(fs::read(&fx.path).unwrap(), before);

    fs::write(&path, "CREATE (:CHARACTER {kala_handle:'unseen',name:'Semi;colon'}) AT TICK 800;\nCREATE (:CHARACTER {kala_handle:'unseen'});\nCREATE (:CHARACTER {kala_handle:'later'});").unwrap();
    let o = transact(&fx.path, &path, &[]);
    assert_ne!(code(&o), 0);
    let detail = stderr(&o);
    assert!(
        detail.contains("statement 2 failed")
            && detail.contains("1 prior statement")
            && detail.contains("committed"),
        "{detail}"
    );
    let world = kala_core::World::open_read_only(&fx.path).unwrap();
    assert_eq!(world.latest_tx().0, fx.mem.latest_tx().0 + 1);
    assert!(world.entity_by_handle("unseen").is_some());
    assert!(world.entity_by_handle("later").is_none());
}

#[test]
fn query_points_cypher_writes_at_transact() {
    let fx = common::build(include_str!("../../kala-core/tests/fixtures/varrow.yaml"));
    let before = fs::read(&fx.path).unwrap();
    let o = fx.run("query", &["--lang", "cypher", "CREATE (:T)"]);
    assert_ne!(code(&o), 0);
    assert!(
        stderr(&o).contains("kala transact --lang cypher"),
        "{}",
        stderr(&o)
    );
    assert_eq!(fs::read(fx.path).unwrap(), before);
}

#[test]
fn equivalent_writes_produce_identical_dump_text_and_log() {
    for &(cypher, datalog) in write_cases::CASES {
        let fx = common::build(include_str!("../../kala-core/tests/fixtures/varrow.yaml"));
        let copy = fx.dir.path().join("cypher.kala");
        fs::copy(&fx.path, &copy).unwrap();
        let input = fx.dir.path().join("operations");
        fs::write(&input, format!("{cypher};")).unwrap();
        let o = transact(&copy, &input, &[]);
        assert_eq!(code(&o), 0, "{cypher}: {}", stderr(&o));
        fs::write(&input, datalog).unwrap();
        let o = fx.run("transact", &[input.to_str().unwrap()]);
        assert_eq!(code(&o), 0, "{datalog}: {}", stderr(&o));
        let a = fx.run("dump", &[]);
        let b = kala(["dump", copy.to_str().unwrap()]);
        assert_eq!(code(&a), 0, "{}", stderr(&a));
        assert_eq!(code(&b), 0, "{}", stderr(&b));
        assert_eq!(stdout(&a), stdout(&b), "{cypher}");
        assert!(
            fs::read(&fx.path).unwrap() == fs::read(&copy).unwrap(),
            "{cypher}"
        );
    }
}

#[test]
fn write_return_in_json_includes_committed_ids_and_dry_run_preview() {
    let fx = common::varrow();
    let input = fx.dir.path().join("return.cypher");
    fs::write(&input, "CREATE (n:CHARACTER {kala_handle:'preview',name:'Aster'}) AT TICK 901 RETURN n AS person, n.name AS name;").unwrap();
    let before = fs::read(&fx.path).unwrap();
    for (extra, dry_run) in [
        (&["--json", "--dry-run"][..], true),
        (&["--json"][..], false),
    ] {
        let out = transact(&fx.path, &input, extra);
        assert_eq!(code(&out), 0, "{}", stderr(&out));
        let record: Value = serde_json::from_slice(&out.stdout).unwrap();
        let columns = record["columns"].as_array().expect("RETURN column names");
        assert_eq!(columns.len(), 2);
        assert!(columns[0].as_str().unwrap().ends_with("person"));
        assert!(columns[1].as_str().unwrap().ends_with("name"));
        let row = &record["rows"][0];
        assert_eq!(row[0]["type"], "ref");
        assert_eq!(row[0]["value"]["handle"], "preview");
        assert_eq!(row[1]["value"], "Aster");
        assert_eq!(record["dry_run"], dry_run);
        if dry_run {
            assert_eq!(fs::read(&fx.path).unwrap(), before);
        } else {
            let w = kala_core::World::open_read_only(&fx.path).unwrap();
            let id = w.entity_by_handle("preview").unwrap();
            assert_eq!(row[0]["value"]["id"], w.format_id(id));
        }
    }
}

#[test]
fn cli_imports_parallel_lists_and_repeating_the_script_keeps_identical_bytes() {
    let fx = common::varrow();
    let input = fx.dir.path().join("parallel.cypher");
    fs::write(&input,
        "WITH ['parallel_a','parallel_b'] AS handles, ['Aster','Briar'] AS names\n\
         UNWIND [1,0] AS i MERGE (n:CHARACTER {kala_handle:handles[i]}) ON CREATE SET n.name=names[i] AT TICK 813;"
    ).unwrap();
    let args = ["--lang", "cypher", input.to_str().unwrap()];
    let first = fx.json("transact", &args);
    assert_eq!(first["changed"], 2);
    let w = kala_core::World::open_read_only(&fx.path).unwrap();
    for (h, name) in [("parallel_a", "Aster"), ("parallel_b", "Briar")] {
        assert_eq!(
            w.as_of(w.entity_by_handle(h).unwrap(), "name", kala_core::Tick(813))
                .unwrap(),
            [kala_core::Value::Str(name.into())]
        );
    }
    let bytes = fs::read(&fx.path).unwrap();
    let again = fx.json("transact", &args);
    assert_eq!(again["changed"], 0);
    assert_eq!(fs::read(&fx.path).unwrap(), bytes);
}

#[test]
fn bulk_params_transact_map_rows_parallel_lists_and_datalog_write_identical_logs() {
    let fx = common::varrow();
    let map_path = fx.dir.path().join("maps.kala");
    let list_path = fx.dir.path().join("lists.kala");
    for path in [&map_path, &list_path] {
        fs::copy(&fx.path, path).unwrap();
    }
    let input = fx.dir.path().join("bulk");
    fs::write(&input, "UNWIND $rows AS row MERGE (n:CHARACTER {kala_handle:row.handle}) SET n.name=row.name AT TICK row.tick;").unwrap();
    let output = transact(
        &map_path,
        &input,
        &[
            "--param",
            r#"rows=[{"handle":"rook","name":"Rook","tick":-17},{"handle":"finch","name":"Finch","tick":9}]"#,
        ],
    );
    assert_eq!(code(&output), 0, "{}", stderr(&output));
    fs::write(&input, "UNWIND $indices AS i MERGE (n:CHARACTER {kala_handle:$handles[i]}) SET n.name=$names[i] AT TICK $ticks[i];").unwrap();
    let output = transact(
        &list_path,
        &input,
        &[
            "--param",
            "indices=[0,1]",
            "--param",
            r#"handles=["rook","finch"]"#,
            "--param",
            r#"names=["Rook","Finch"]"#,
            "--param",
            "ticks=[-17,9]",
        ],
    );
    assert_eq!(code(&output), 0, "{}", stderr(&output));
    fs::write(&input, "[[:create :CHARACTER ?a :if-missing] [:set ?a :name ?na :at ?ta] [:create :CHARACTER ?b :if-missing] [:set ?b :name ?nb :at ?tb]]").unwrap();
    let output = fx.run(
        "transact",
        &[
            input.to_str().unwrap(),
            "--param",
            "a=rook",
            "--param",
            "b=finch",
            "--param",
            "na=Rook",
            "--param",
            "nb=Finch",
            "--param",
            "ta=-17",
            "--param",
            "tb=9",
        ],
    );
    assert_eq!(code(&output), 0, "{}", stderr(&output));
    let expected = fs::read(&fx.path).unwrap();
    for other in [map_path, list_path] {
        assert!(
            fs::read(other).unwrap() == expected,
            "write paths appended different entries"
        );
    }
}
