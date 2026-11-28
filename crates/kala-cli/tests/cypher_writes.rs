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

#[test]
fn bulk_params_text_value_type_follows_an_attribute_declared_in_the_same_form() {
    let fx = common::varrow();
    let input = fx.dir.path().join("parameter-types");
    fs::write(
        &input,
        "[[:attribute :projector_code :string] [:assert :greg :projector_code ?v :at 603]]",
    )
    .unwrap();
    let output = fx.run("transact", &[input.to_str().unwrap(), "--param", "v=123"]);
    assert_eq!(code(&output), 0, "{}", stderr(&output));
    let world = kala_core::World::open_read_only(&fx.path).unwrap();
    let greg = world.entity_by_handle("greg").unwrap();
    assert_eq!(
        world
            .as_of(greg, "projector_code", kala_core::Tick(603))
            .unwrap(),
        [kala_core::Value::Str("123".into())]
    );
}

#[test]
fn bulk_params_text_value_type_follows_the_parameterized_correction_target() {
    let fx = common::varrow();
    let input = fx.dir.path().join("correction");
    fs::write(&input, r#"[:assert :greg :projector_code "123" :at 603]"#).unwrap();
    let setup = fx.run("transact", &[input.to_str().unwrap()]);
    assert_eq!(code(&setup), 0, "{}", stderr(&setup));
    let target = {
        let world = kala_core::World::open_read_only(&fx.path).unwrap();
        let greg = world.entity_by_handle("greg").unwrap();
        assert_eq!(
            world
                .as_of(greg, "projector_code", kala_core::Tick(603))
                .unwrap(),
            [kala_core::Value::Str("123".into())]
        );
        world.entries().find(|entry| matches!(&entry.body,
            kala_core::log::EntryBody::Fact {entity, tick: kala_core::Tick(603), value: kala_core::Value::Str(s), ..}
            if *entity == greg && s == "123")).unwrap().id
    };
    fs::write(&input, "[:correct ?entry :replace :value ?v]").unwrap();
    let output = fx.run(
        "transact",
        &[
            input.to_str().unwrap(),
            "--param",
            &format!("entry={}", target.0),
            "--param",
            "v=456",
        ],
    );
    assert_eq!(code(&output), 0, "{}", stderr(&output));
    let world = kala_core::World::open_read_only(&fx.path).unwrap();
    let greg = world.entity_by_handle("greg").unwrap();
    assert_eq!(
        world
            .as_of(greg, "projector_code", kala_core::Tick(603))
            .unwrap(),
        [kala_core::Value::Str("456".into())]
    );
}

#[test]
fn a_row_field_compared_with_a_ref_attribute_names_an_entity_like_a_scalar_parameter() {
    let fx = common::varrow();
    let count = |query: &str, param: &str| {
        fx.json("query", &["--lang", "cypher", query, "--param", param])["rows"][0][0]["value"]
            .clone()
    };
    let scalar = count(
        "MATCH (n:CHARACTER) WHERE n.located_in = $p RETURN count(*)",
        "p=varrow",
    );
    assert_eq!(scalar, 4);
    for query in [
        "UNWIND $rows AS row MATCH (n:CHARACTER) WHERE n.located_in = row.place RETURN count(*)",
        "UNWIND $rows AS row MATCH (n:CHARACTER) WHERE row.place = n.located_in RETURN count(*)",
        "UNWIND $rows AS row MATCH (n:CHARACTER {located_in: row.place}) RETURN count(*)",
    ] {
        assert_eq!(
            count(query, r#"rows=[{"place":"varrow"}]"#),
            scalar,
            "{query}"
        );
        // A formatted ID names the same entity, and NULL matches nothing.
        let id = fx.mem.format_id(fx.mem.entity_by_handle("varrow").unwrap());
        assert_eq!(
            count(query, &format!(r#"rows=[{{"place":"{id}"}}]"#)),
            scalar,
            "{query}"
        );
        assert_eq!(count(query, r#"rows=[{"place":null}]"#), 0, "{query}");
        // An unknown handle is the scalar parameter's error, never an empty result.
        let unknown = fx.fails(
            1,
            "query",
            &[
                "--lang",
                "cypher",
                query,
                "--param",
                r#"rows=[{"place":"nowhere"}]"#,
            ],
        );
        assert!(
            unknown.contains("wrong type for query parameter `$rows`: expected an entity"),
            "{query}: {unknown}"
        );
    }
    // The scalar parameter's refusal is the same binding error (its text can be quoted back).
    let scalar_unknown = fx.fails(
        1,
        "query",
        &[
            "--lang",
            "cypher",
            "MATCH (n:CHARACTER) WHERE n.located_in = $p RETURN count(*)",
            "--param",
            "p=nowhere",
        ],
    );
    assert!(
        scalar_unknown.contains("unknown entity `nowhere`"),
        "{scalar_unknown}"
    );

    // The write form: SET on the nodes the row's place selects.
    let input = fx.dir.path().join("by-place.cypher");
    fs::write(
        &input,
        "UNWIND $rows AS row MATCH (n:CHARACTER {located_in: row.place}) SET n.census = row.year AT TICK 700;",
    )
    .unwrap();
    let report = fx.json(
        "transact",
        &[
            "--lang",
            "cypher",
            input.to_str().unwrap(),
            "--param",
            r#"rows=[{"place":"varrow","year":700}]"#,
        ],
    );
    assert_eq!(report["changed"], 4);

    // One field cannot be both an entity and a plain string.
    let both = fx.fails(
        1,
        "query",
        &[
            "--lang",
            "cypher",
            "UNWIND $rows AS row MATCH (n:CHARACTER) WHERE n.located_in = row.x AND n.name = row.x RETURN count(*)",
            "--param",
            r#"rows=[{"x":"varrow"}]"#,
        ],
    );
    assert!(
        both.contains("row field") && both.contains("entity") && both.contains("x"),
        "{both}"
    );
}

#[test]
fn cypher_text_parameters_that_only_look_like_json_stay_strings() {
    let fx = common::varrow();
    let input = fx.dir.path().join("note.cypher");
    fs::write(
        &input,
        "MATCH (n:CHARACTER {kala_handle:'greg'}) SET n.note = $note AT TICK 700;",
    )
    .unwrap();
    for (tick, note) in ["{draft}", " [x", "{\"a\":1}", " \"quoted\""]
        .iter()
        .enumerate()
    {
        // Read: the text is compared as a string, not refused as bad JSON.
        let found = fx.json(
            "query",
            &[
                "--lang",
                "cypher",
                "MATCH (n:CHARACTER) WHERE n.name = $x RETURN count(*)",
                "--param",
                &format!("x={note}"),
            ],
        );
        assert_eq!(found["rows"][0][0]["value"], 0, "{note}");
        // Write: it is stored exactly as typed.
        fs::write(
            &input,
            format!(
                "MATCH (n:CHARACTER {{kala_handle:'greg'}}) SET n.note = $note AT TICK {};",
                700 + tick
            ),
        )
        .unwrap();
        let o = transact(&fx.path, &input, &["--param", &format!("note={note}")]);
        assert_eq!(code(&o), 0, "{note}: {}", stderr(&o));
        let w = kala_core::World::open_read_only(&fx.path).unwrap();
        let greg = w.entity_by_handle("greg").unwrap();
        assert_eq!(
            w.as_of(greg, "note", kala_core::Tick(700 + tick as i64))
                .unwrap(),
            [kala_core::Value::Str((*note).into())],
            "{note}"
        );
    }
    // What was JSON before still is: NULL, a list, a quoted string.
    let count = |query: &str, param: &str| {
        fx.json("query", &["--lang", "cypher", query, "--param", param])["rows"][0][0]["value"]
            .clone()
    };
    let by_list = "UNWIND $hs AS h MATCH (n:CHARACTER {kala_handle: h}) RETURN count(*)";
    assert_eq!(count(by_list, r#"hs=["greg","edwin"]"#), 2);
    let by_handle = "MATCH (n:CHARACTER) WHERE n.kala_handle = $h RETURN count(*)";
    assert_eq!(count(by_handle, r#"h="greg""#), 1);
    assert_eq!(count(by_handle, "h=null"), 0);
    let bad = fx.fails(
        1,
        "query",
        &["--lang", "cypher", by_list, "--param", "hs=[x"],
    );
    assert!(bad.contains("invalid JSON"), "{bad}");
    // A map-row source is JSON even after leading whitespace.
    let by_rows = "UNWIND $rows AS row MATCH (n:CHARACTER {kala_handle: row.h}) RETURN count(*)";
    assert_eq!(count(by_rows, r#"rows= [{"h":"greg"}]"#), 1);
    let map = fx.fails(
        1,
        "query",
        &[
            "--lang",
            "cypher",
            by_rows,
            "--param",
            r#"rows={"h":"greg"}"#,
        ],
    );
    assert!(
        map.contains("a map is only allowed in a list of rows"),
        "{map}"
    );
}

#[test]
fn a_refused_text_parameter_names_the_parameter_its_kind_and_the_offending_text() {
    let fx = common::varrow();
    let query = |text: &str, param: &str| fx.fails(1, "query", &[text, "--param", param]);
    let allies = "[:find ?e :in ?who :where [?e :ally_of ?who]]";
    assert_eq!(
        query(allies, "who=gregg").trim_end(),
        "kala: error: unknown entity `gregg`: not a handle or an ID of this world (parameter `?who`)"
    );
    assert_eq!(
        query("[:find ?e :in ?t :at ?t :where [?e :ally_of _]]", "t=abc").trim_end(),
        "kala: error: `abc` is not a tick (a signed 64-bit integer) for parameter `?t`"
    );
    assert_eq!(
        query(
            "[:find ?e :in ?c :canon ?c :where [?e :ally_of _]]",
            "c=soon"
        )
        .trim_end(),
        "kala: error: `soon` is not a canon (`tx#N` or `N`) for parameter `?c`"
    );
    let unknown = query(allies, "whom=greg");
    assert!(unknown.contains("unknown parameter `?whom`"), "{unknown}");
    let twice = fx.fails(
        1,
        "query",
        &[allies, "--param", "who=greg", "--param", "?who=greg"],
    );
    assert!(twice.contains("parameter `?who` is given twice"), "{twice}");
    let missing = fx.fails(1, "query", &[allies]);
    assert!(missing.contains("missing parameter `?who`"), "{missing}");

    // Cypher names its parameters with `$`.
    let cypher = |text: &str, param: &str| {
        fx.fails(1, "query", &["--lang", "cypher", text, "--param", param])
    };
    let located = "MATCH (n:CHARACTER) WHERE n.located_in = $p RETURN count(*)";
    assert_eq!(
        cypher(located, "p=nowhere").trim_end(),
        "kala: error: unknown entity `nowhere`: not a handle or an ID of this world (parameter `$p`)"
    );
    // A JSON value is not echoed: the message names the parameter and its kind.
    let list = cypher(located, r#"p=[1]"#);
    assert!(
        list.contains("parameter `$p`") && list.contains("an entity"),
        "{list}"
    );

    // A write form's parameter is refused the same way.
    let input = fx.dir.path().join("tick");
    fs::write(&input, "[:assert :greg :alias \"G\" :at ?t]").unwrap();
    let o = fx.fails(
        1,
        "transact",
        &[input.to_str().unwrap(), "--param", "t=abc"],
    );
    assert!(
        o.contains("`abc` is not a tick (a signed 64-bit integer) for parameter `?t`"),
        "{o}"
    );
}
