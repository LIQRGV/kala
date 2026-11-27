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
