#[path = "common/client.rs"]
mod client;
mod common;
use client::{ALLIES, Client, map, string, structure};
use common::Running;
use kala_bolt::{ServerConfig, packstream::Value};
use std::collections::BTreeMap;

fn node(value: &Value) -> (Vec<Value>, BTreeMap<String, Value>, String) {
    let fields = structure(value.clone(), 0x4e);
    assert_eq!(fields.len(), 4);
    assert!(matches!(fields[0], Value::Integer(n) if n > 0));
    let Value::List(labels) = &fields[1] else {
        panic!("labels must be a list")
    };
    let Value::Map(properties) = &fields[2] else {
        panic!("properties must be a map")
    };
    let Value::String(element_id) = &fields[3] else {
        panic!("element_id must be a string")
    };
    (labels.clone(), properties.clone(), element_id.clone())
}

#[test]
fn datalog_nodes_have_types_handles_sorted_properties_and_temporal_labels() {
    let server = Running::start();
    let mut client = Client::ready(&server);
    let rows = client.query(ALLIES);
    let mut names = vec![];
    for row in rows {
        let (labels, props, element_id) = node(&row[0]);
        assert!(labels.contains(&string("CHARACTER")));
        assert!(element_id.starts_with("CHARACTER#"));
        assert!(props.contains_key("kala_handle"));
        assert!(!props.contains_key("handle"));
        assert!(!props.contains_key("located_in"));
        names.push(props["name"].clone());
    }
    assert_eq!(names, [string("Greg"), string("Tomas")]);
    let rows =
        client.query("// kala:datalog   \n[:find ?e :at -210 :where [?e :kala_handle \"edwin\"]]");
    let (labels, props, _) = node(&rows[0][0]);
    assert!(labels.contains(&string("Knight")));
    assert!(!labels.contains(&string("King")));
    assert_eq!(props["name"], string("Edwin"));
    assert_eq!(
        props["alias"],
        Value::List(vec![
            string("Ed"),
            string("Eddie"),
            string("the Lost Knight")
        ])
    );
    let rows =
        client.query("// kala:datalog\n[:find ?e :at -180 :where [?e :kala_handle \"edwin\"]]");
    let (labels, props, _) = node(&rows[0][0]);
    assert!(labels.contains(&string("King")));
    assert_eq!(props["name"], string("the Grey King"));
    assert_eq!(
        props["alias"],
        Value::List(vec![string("Ed"), string("Eddie")])
    );
    let rows = client
        .query("// kala:datalog\n[:find ?e :at -210 :canon 5 :where [?e :kala_handle \"edwin\"]]");
    let (_, props, _) = node(&rows[0][0]);
    assert_eq!(
        props["alias"],
        Value::List(vec![
            string("Ed"),
            string("Eddy"),
            string("the Lost Knight")
        ])
    );
}

#[test]
fn node_keeps_world_handle_separate_from_user_handle_property() {
    let dir = tempfile::tempdir().unwrap();
    let yaml = r#"
schema:
  node_types: [CHARACTER]
  attributes:
    - { name: handle, type: string }
entities:
  CHARACTER: [alice]
transactions:
  - tick: 0
    ops:
      - assert: [alice, handle, user-alias]
"#;
    drop(kala_fixture::load_fixture_to(yaml, &dir.path().join("typed.kala")).unwrap());
    let server = Running::in_dir(
        dir,
        ServerConfig {
            listen: vec!["127.0.0.1:0".parse().unwrap()],
            ..ServerConfig::default()
        },
    );
    let mut client = Client::ready(&server);
    let rows = client.query("// kala:datalog\n[:find ?e :where [?e :handle \"user-alias\"]]");
    assert_eq!(rows.len(), 1);
    let (_, props, _) = node(&rows[0][0]);
    assert_eq!(props["kala_handle"], string("alice"));
    assert_eq!(props["handle"], string("user-alias"));
    let builtin = client.query("// kala:datalog\n[:find ?e :where [?e :kala_handle \"alice\"]]");
    assert_eq!(rows, builtin);
}

#[test]
fn relationships_have_bolt5_layout_and_properties_at_result_tick() {
    let server = Running::start();
    let mut client = Client::ready(&server);
    for (tick, disguised) in [(-230, true), (-210, false)] {
        let rows = client.query(&format!(
            "// kala:datalog\n[:find ?e :at {tick} :where [?e :disguised ?d]]"
        ));
        assert_eq!(rows.len(), 1);
        let fields = structure(rows[0][0].clone(), 0x52);
        assert_eq!(fields.len(), 8);
        assert!(matches!(fields[0], Value::Integer(n) if n < 0));
        assert_eq!(fields[3], string("present"));
        assert_eq!(fields[4], map(&[("disguised", Value::Bool(disguised))]));
        assert!(matches!(&fields[5], Value::String(s) if s.starts_with("edge#")));
        assert!(matches!(&fields[6], Value::String(s) if s.starts_with("CHARACTER#")));
        assert!(matches!(&fields[7], Value::String(s) if s.starts_with("PLACE#")));
    }
}

#[test]
fn parameters_types_missing_extra_entity_and_canon() {
    let server = Running::start();
    let mut client = Client::ready(&server);
    let query = "// kala:datalog\n[:find ?a :in $ ?who ?t :at ?t :where [?e :kala_handle ?who] [?e :ally_of ?a]]";
    client.run(
        query,
        map(&[
            ("who", string("edwin")),
            ("t", Value::Integer(-210)),
            ("unused", Value::Null),
        ]),
        map(&[]),
    );
    assert_eq!(client.pull(-1).0.len(), 2);
    for (params, code) in [
        (
            map(&[("who", string("edwin"))]),
            "Neo.ClientError.Statement.ParameterMissing",
        ),
        (
            map(&[("who", string("edwin")), ("t", string("-210"))]),
            "Neo.ClientError.Statement.TypeError",
        ),
        (
            map(&[("who", Value::Null), ("t", Value::Integer(-210))]),
            "Neo.ClientError.Statement.TypeError",
        ),
    ] {
        client.send(0x10, vec![string(query), params, map(&[])]);
        client.failure(code);
        client.reset();
        assert_eq!(client.query(ALLIES).len(), 2);
    }
    let entity_query = "// kala:datalog\n[:find ?a :in $ ?who ?canon :at -210 :canon ?canon :where [?who :ally_of ?a]]";
    for who in ["edwin", "CHARACTER#1"] {
        for canon in [Value::Integer(5), string("tx#5"), string("5")] {
            client.run(
                entity_query,
                map(&[("who", string(who)), ("canon", canon)]),
                map(&[]),
            );
            assert_eq!(client.pull(-1).0.len(), 2);
        }
    }
}

#[test]
fn bool_float_integer_string_parameters_keep_their_types() {
    let dir = tempfile::tempdir().unwrap();
    let yaml = r#"
schema:
  node_types: [CHARACTER]
  attributes:
    - { name: age, type: i64 }
    - { name: height, type: f64 }
    - { name: brave, type: bool }
    - { name: name, type: string }
entities:
  CHARACTER: [alice]
transactions:
  - tick: 0
    ops:
      - assert: [alice, age, 7]
      - assert: [alice, height, 1.5]
      - assert: [alice, brave, true]
      - assert: [alice, name, Alice]
"#;
    drop(kala_fixture::load_fixture_to(yaml, &dir.path().join("typed.kala")).unwrap());
    let server = Running::in_dir(
        dir,
        ServerConfig {
            listen: vec!["127.0.0.1:0".parse().unwrap()],
            ..ServerConfig::default()
        },
    );
    let mut client = Client::ready(&server);
    for (attr, value) in [
        ("age", Value::Integer(7)),
        ("height", Value::Float(1.5)),
        ("brave", Value::Bool(true)),
        ("name", string("Alice")),
    ] {
        let q = format!("// kala:datalog\n[:find ?e :in $ ?value :where [?e :{attr} ?value]]");
        client.run(&q, map(&[("value", value.clone())]), map(&[]));
        let rows = client.pull(-1).0;
        assert_eq!(rows.len(), 1);
        assert_eq!(node(&rows[0][0]).1[attr], value);
        client.send(
            0x10,
            vec![string(&q), map(&[("value", Value::Null)]), map(&[])],
        );
        client.failure("Neo.ClientError.Statement.TypeError");
        client.reset();
    }
}

#[test]
fn aggregates_history_precision_intervals_and_extreme_ticks() {
    let server = Running::start();
    let mut client = Client::ready(&server);
    let fields = client
        .run_empty("// kala:datalog\n[:find (count ?c) :at -215 :where [?c :located_in :varrow]]");
    assert_eq!(fields["fields"], Value::List(vec![string("(count ?c)")]));
    assert_eq!(client.pull(-1).0, vec![vec![Value::Integer(5)]]);
    let rows = client.query("// kala:datalog\n[:find ?t ?op ?p :where [:mara :name ?n ?t ?op ?p]]");
    assert!(rows.contains(&vec![
        Value::Integer(-100),
        string("retract"),
        string("decade")
    ]));
    assert!(rows.iter().all(|row| matches!(row[0], Value::Integer(_))));
    let rows =
        client.query("// kala:datalog\n[:find ?s :where (during [:osric :located_in :varrow] ?s)]");
    assert_eq!(
        rows,
        vec![vec![Value::List(vec![Value::List(vec![
            Value::Integer(-400),
            Value::Null
        ])])]]
    );
    for tick in [i64::MIN, -300, 0, i64::MAX] {
        let q = format!("// kala:datalog\n[:find ?n :at {tick} :where [:edwin :name ?n]]");
        let rows = client.query(&q);
        let expected = match tick {
            i64::MIN => vec![],
            -300 => vec![vec![string("Edwin")]],
            _ => vec![vec![string("the Grey King")]],
        };
        assert_eq!(rows, expected);
    }
}

#[test]
fn syntax_errors_languages_database_names_and_error_hygiene() {
    let server = Running::start();
    let mut client = Client::ready(&server);
    for q in [
        "RETURN 1",
        "// kala:gremlin\ng.V()",
        " // kala:datalog\n[:find ?n :where [:edwin :name ?n]]",
    ] {
        client.send(0x10, vec![string(q), map(&[]), map(&[])]);
        let failure = client.failure("Neo.ClientError.Statement.SyntaxError");
        assert!(matches!(&failure["message"], Value::String(s) if s.contains("not supported yet")));
        client.reset();
    }
    client.send(
        0x10,
        vec![string("// kala:datalog\n[:find"), map(&[]), map(&[])],
    );
    let failure = client.failure("Neo.ClientError.Statement.SyntaxError");
    assert!(
        matches!(&failure["message"], Value::String(s) if s.contains("line") && s.contains("column"))
    );
    client.reset();
    client.send(
        0x10,
        vec![
            string(ALLIES),
            map(&[]),
            map(&[("db", string("nope\u{1b}[31m"))]),
        ],
    );
    let failure = client.failure("Neo.ClientError.Database.DatabaseNotFound");
    assert!(
        matches!(&failure["message"], Value::String(s) if !s.contains('\u{1b}') && !s.contains(server.dir.path().to_str().unwrap()))
    );
    client.reset();
    assert_eq!(client.query(ALLIES).len(), 2);
}

#[test]
fn ambiguous_default_is_refused_and_explicit_default_is_honored() {
    for default in [None, Some("other".into())] {
        let dir = tempfile::tempdir().unwrap();
        for db in ["varrow", "other"] {
            drop(
                kala_fixture::load_fixture_to(
                    common::VARROW,
                    &dir.path().join(format!("{db}.kala")),
                )
                .unwrap(),
            );
        }
        let config = ServerConfig {
            listen: vec!["127.0.0.1:0".parse().unwrap()],
            default_db: default.clone(),
            ..ServerConfig::default()
        };
        let server = Running::in_dir(dir, config);
        let mut client = Client::ready(&server);
        client.send(0x10, vec![string(ALLIES), map(&[]), map(&[])]);
        if default.is_none() {
            client.failure("Neo.ClientError.Database.DatabaseNotFound");
            client.reset();
        } else {
            client.success();
            assert_eq!(client.pull(-1).1["db"], string("other"));
        }
        client.run(ALLIES, map(&[]), map(&[("db", string("varrow"))]));
        assert_eq!(client.pull(-1).0.len(), 2);
    }
}

#[test]
fn external_commit_is_visible_to_next_run_but_not_open_result() {
    let server = Running::start();
    let mut client = Client::ready(&server);
    let query = "// kala:datalog\n[:find ?n :at 700 :where [:greg :alias ?n]]";
    let original = client.query(query);
    client.run_empty(query);
    let mut world = kala_core::World::open(&server.dir.path().join("varrow.kala")).unwrap();
    let greg = world.entity_by_handle("greg").unwrap();
    world
        .assert(
            greg,
            "alias",
            kala_core::Value::Str("Greg the Bold".into()),
            Some(kala_core::Tick(700)),
        )
        .unwrap();
    drop(world);
    assert_eq!(client.pull(-1).0, original);
    let changed = client.query(query);
    assert!(changed.contains(&vec![string("Greg the Bold")]));
    assert_eq!(changed.len(), original.len() + 1);
}

#[test]
fn unreadable_world_is_a_generic_store_error_without_paths() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("broken.kala"), b"not a kala world").unwrap();
    let server = Running::in_dir(
        dir,
        ServerConfig {
            listen: vec!["127.0.0.1:0".parse().unwrap()],
            ..ServerConfig::default()
        },
    );
    let mut client = Client::ready(&server);
    client.send(0x10, vec![string(ALLIES), map(&[]), map(&[])]);
    let failure = client.failure("Neo.DatabaseError.General.UnknownError");
    assert_eq!(failure["message"], string("the world could not be read"));
}

#[test]
fn node_property_larger_than_wire_budget_fails_before_streaming_and_reset_recovers() {
    let dir = tempfile::tempdir().unwrap();
    let yaml = format!(
        "schema:\n  node_types: [CHARACTER]\n  attributes:\n    - {{ name: name, type: string }}\n    - {{ name: biography, type: string, indexed: false }}\nentities:\n  CHARACTER: [edwin]\ntransactions:\n  - tick: 0\n    ops:\n      - assert: [edwin, name, Edwin]\n      - assert: [edwin, biography, \"{}\"]\n",
        "x".repeat(1024 * 1024)
    );
    drop(kala_fixture::load_fixture_to(&yaml, &dir.path().join("large.kala")).unwrap());
    let server = Running::in_dir(
        dir,
        ServerConfig {
            listen: vec!["127.0.0.1:0".parse().unwrap()],
            ..ServerConfig::default()
        },
    );
    let mut client = Client::ready(&server);
    client.send(
        0x10,
        vec![
            string("// kala:datalog\n[:find ?e :where [?e :name \"Edwin\"]]"),
            map(&[]),
            map(&[]),
        ],
    );
    let failure = client.failure("Neo.ClientError.Statement.ExecutionFailed");
    assert!(
        matches!(&failure["message"], Value::String(s) if s.contains("limit") || s.contains("budget"))
    );
    client.reset();
    assert_eq!(
        client.query("// kala:datalog\n[:find ?name :where [:edwin :name ?name]]"),
        vec![vec![string("Edwin")]]
    );
}
