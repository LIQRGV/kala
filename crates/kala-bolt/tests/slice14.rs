//! Slice 14 uses real sockets: routing, paths, procedures and secret refusal.
#[path = "common/client.rs"]
mod client;
mod common;
use client::{Client, map, metadata, string, structure};
use common::{Running, assert_closed};
use kala_bolt::packstream::Value;
use kala_bolt::{Server, ServerConfig};
use kala_system::system;
use std::collections::BTreeMap;

const INVALID: &str = "Neo.ClientError.Request.Invalid";
const FORBIDDEN: &str = "Neo.ClientError.Security.Forbidden";

fn fields(db: Value) -> Vec<Value> {
    vec![
        map(&[("address", string("untrusted.invalid:9999"))]),
        Value::List(vec![]),
        map(&[("db", db)]),
    ]
}

fn account_server() -> Running {
    let dir = tempfile::tempdir().unwrap();
    for name in ["varrow", "hidden"] {
        drop(
            kala_fixture::load_fixture_to(common::VARROW, &dir.path().join(format!("{name}.kala")))
                .unwrap(),
        );
    }
    system::add(dir.path(), "alice@localhost", None).unwrap();
    system::grant(dir.path(), "alice@localhost", "read", "varrow").unwrap();
    let config = ServerConfig {
        listen: "127.0.0.1:0".parse().unwrap(),
        default_db: Some("varrow".into()),
        ..ServerConfig::default()
    };
    let server = Server::bind_accounts(dir.path(), config).unwrap();
    Running::from_server(dir, server)
}

fn alice(server: &Running) -> Client {
    login(server, "alice")
}

fn login(server: &Running, user: &str) -> Client {
    let mut c = Client::connected(server, 4);
    c.hello();
    c.success();
    c.send(
        0x6a,
        vec![map(&[
            ("scheme", string("basic")),
            ("principal", string(user)),
            ("credentials", string("")),
        ])],
    );
    c.success();
    c
}

#[test]
fn route_reports_reached_socket_in_every_role_without_context_reflection() {
    let server = Running::start();
    let mut c = Client::ready(&server);
    for db in [Value::Null, string("varrow")] {
        c.send(0x66, fields(db));
        let meta = c.success();
        assert_eq!(
            meta["rt"],
            map(&[
                ("ttl", Value::Integer(300)),
                ("db", string("varrow")),
                (
                    "servers",
                    Value::List(
                        ["WRITE", "READ", "ROUTE"]
                            .into_iter()
                            .map(|role| map(&[
                                (
                                    "addresses",
                                    Value::List(vec![string(&server.addr.to_string())])
                                ),
                                ("role", string(role))
                            ]))
                            .collect()
                    )
                )
            ])
        );
    }
    assert_eq!(c.query(client::ALLIES).len(), 2);
}

#[test]
fn advertised_address_is_used_for_route_and_database_metadata() {
    let address = "graph.example.test:17687";
    let server = Running::with_config(ServerConfig {
        advertised_address: Some(address.into()),
        ..ServerConfig::default()
    });
    let mut c = Client::ready(&server);
    c.send(0x66, fields(Value::Null));
    let meta = c.success();
    let Value::Map(rt) = &meta["rt"] else {
        panic!("routing table")
    };
    let Value::List(servers) = &rt["servers"] else {
        panic!("servers")
    };
    assert_eq!(servers.len(), 3);
    for server in servers {
        let Value::Map(server) = server else {
            panic!("server")
        };
        assert_eq!(server["addresses"], Value::List(vec![string(address)]));
    }
    assert_eq!(
        c.query("SHOW DATABASES YIELD address"),
        vec![vec![string(address)]]
    );
}

#[test]
fn route_is_ready_only_authenticated_and_reset_recovers() {
    let server = Running::start();
    for hello in [false, true] {
        let mut c = Client::connected(&server, 4);
        if hello {
            c.hello();
            c.success();
        }
        c.send(0x66, fields(Value::Null));
        c.failure(INVALID);
        assert_closed(&mut c.stream);
    }
    let mut c = Client::ready(&server);
    c.run_empty(client::ALLIES);
    c.send(0x66, fields(Value::Null));
    c.failure(INVALID);
    c.send(0x66, fields(Value::Null));
    assert!(structure(c.recv(), 0x7e).is_empty());
    c.reset();
    c.send(0x66, fields(Value::Null));
    c.success();
}

#[test]
fn route_malformed_fields_fail_and_impersonation_is_forbidden() {
    let server = Running::start();
    let mut c = Client::ready(&server);
    for fields in [
        vec![],
        vec![map(&[]), Value::List(vec![])],
        vec![Value::Null, Value::List(vec![]), map(&[])],
        vec![
            map(&[("address", Value::Integer(1))]),
            Value::List(vec![]),
            map(&[]),
        ],
        vec![map(&[]), Value::Null, map(&[])],
        vec![map(&[]), Value::List(vec![Value::Integer(1)]), map(&[])],
        vec![map(&[]), Value::List(vec![]), Value::Null],
        vec![
            map(&[]),
            Value::List(vec![]),
            map(&[("db", Value::Integer(1))]),
        ],
        vec![
            map(&[]),
            Value::List(vec![]),
            map(&[("imp_user", Value::Integer(1))]),
        ],
    ] {
        c.send(0x66, fields);
        c.failure(INVALID);
        c.reset();
    }
    c.send(
        0x66,
        vec![
            map(&[]),
            Value::List(vec![]),
            map(&[("db", string("varrow")), ("imp_user", string("root"))]),
        ],
    );
    c.failure(FORBIDDEN);
    c.reset();
}

#[test]
fn route_unknown_and_forbidden_database_failures_equal_run() {
    let server = account_server();
    for mut c in [alice(&server), Client::ready(&server)] {
        for db in ["hidden", "does-not-exist", "system"] {
            c.send(
                0x10,
                vec![
                    string("MATCH (n) RETURN n LIMIT 0"),
                    map(&[]),
                    map(&[("db", string(db))]),
                ],
            );
            let result = c.recv();
            if matches!(result, Value::Structure(0x70, _)) {
                c.pull(-1);
                c.send(0x66, fields(string(db)));
                c.success();
            } else {
                let expected = metadata(result, 0x7f);
                c.reset();
                c.send(0x66, fields(string(db)));
                let actual = metadata(c.recv(), 0x7f);
                assert_eq!(actual["code"], expected["code"], "{db}");
                assert_eq!(actual["message"], expected["message"], "{db}");
                c.reset();
            }
        }
    }
}

#[test]
fn procedure_rows_yield_pipeline_and_unknown_procedure_codes() {
    let server = Running::start();
    let mut c = Client::ready(&server);
    assert_eq!(
        c.query("CALL dbms.components()"),
        vec![vec![
            string("Kala"),
            Value::List(vec![string("5.0.0")]),
            string("community")
        ]]
    );
    assert_eq!(
        c.query("CALL db.labels() YIELD label WHERE label = 'City' RETURN label"),
        vec![vec![string("City")]]
    );
    for (query, column, wanted, absent) in [
        ("CALL db.labels()", "label", "CHARACTER", "missing"),
        (
            "CALL db.relationshipTypes()",
            "relationshipType",
            "ally_of",
            "name",
        ),
        (
            "CALL db.propertyKeys()",
            "propertyKey",
            "kala_handle",
            "ally_of",
        ),
    ] {
        assert_eq!(
            c.run_empty(query)["fields"],
            Value::List(vec![string(column)])
        );
        let rows = c.pull(-1).0;
        assert!(rows.contains(&vec![string(wanted)]));
        assert!(!rows.contains(&vec![string(absent)]));
    }
    c.send(0x10, vec![string("CALL db.nosuch()"), map(&[]), map(&[])]);
    let failure = c.failure("Neo.ClientError.Procedure.ProcedureNotFound");
    assert!(matches!(&failure["message"], Value::String(s) if s.contains("db.nosuch")));
    c.reset();
    assert_eq!(
        c.query("CALL dbms.components() YIELD name AS n WHERE n = 'Kala' WITH n RETURN n"),
        vec![vec![string("Kala")]]
    );
}

fn show(c: &mut Client, db: &str, query: &str) -> (Vec<String>, Vec<Vec<Value>>) {
    let meta = c.run(query, map(&[]), map(&[("db", string(db))]));
    let Value::List(cols) = &meta["fields"] else {
        panic!("columns")
    };
    let cols = cols
        .iter()
        .map(|c| match c {
            Value::String(s) => s.clone(),
            other => panic!("{other:?}"),
        })
        .collect();
    (cols, c.pull(-1).0)
}

#[test]
fn show_lists_only_granted_databases_including_when_targeting_system() {
    let server = account_server();
    let mut user = alice(&server);
    let columns = [
        "name",
        "type",
        "aliases",
        "access",
        "address",
        "role",
        "writer",
        "requestedStatus",
        "currentStatus",
        "statusMessage",
        "default",
        "home",
        "constituents",
    ];
    for db in ["varrow", "system"] {
        let (cols, rows) = show(&mut user, db, "SHOW DATABASES");
        assert_eq!(cols, columns);
        assert_eq!(
            rows,
            vec![vec![
                string("varrow"),
                string("standard"),
                Value::List(vec![]),
                string("read-only"),
                string(&server.addr.to_string()),
                string("primary"),
                Value::Bool(true),
                string("online"),
                string("online"),
                string(""),
                Value::Bool(true),
                Value::Bool(true),
                Value::List(vec![])
            ]]
        );
        for q in [
            "SHOW DATABASE varrow YIELD name",
            "SHOW DEFAULT DATABASE YIELD name",
            "SHOW HOME DATABASE YIELD name",
            "SHOW DATABASES YIELD name WHERE name = 'varrow'",
        ] {
            assert_eq!(show(&mut user, db, q).1, vec![vec![string("varrow")]]);
        }
        assert!(show(&mut user, db, "SHOW DATABASE hidden").1.is_empty());
        assert!(
            show(&mut user, db, "SHOW DATABASE does_not_exist")
                .1
                .is_empty()
        );
    }
    let mut root = Client::ready(&server);
    assert_eq!(
        show(&mut root, "system", "SHOW DATABASES YIELD name").1,
        vec![
            vec![string("hidden")],
            vec![string("system")],
            vec![string("varrow")]
        ]
    );
    assert_eq!(
        show(&mut root, "system", "SHOW DATABASE system YIELD type").1,
        vec![vec![string("system")]]
    );
}

#[test]
fn show_visibility_includes_any_grant_without_reading_target_world() {
    let server = account_server();
    system::add(server.dir.path(), "writer@localhost", None).unwrap();
    system::grant(server.dir.path(), "writer@localhost", "write", "hidden").unwrap();
    let mut c = login(&server, "writer");
    assert_eq!(
        show(&mut c, "system", "SHOW DATABASES YIELD name, default, home").1,
        vec![vec![
            string("hidden"),
            Value::Bool(false),
            Value::Bool(false)
        ]]
    );
    c.send(
        0x10,
        vec![
            string("MATCH (n) RETURN n"),
            map(&[]),
            map(&[("db", string("hidden"))]),
        ],
    );
    c.failure(FORBIDDEN);
}

#[test]
fn show_authorization_exception_cannot_admit_world_reads_or_other_sources() {
    let server = account_server();
    let mut c = alice(&server);
    for query in [
        "SHOW DATABASES YIELD name MATCH (a:ACCOUNT) RETURN a.user",
        "SHOW DATABASES YIELD name OPTIONAL MATCH (a:ACCOUNT) RETURN a",
        "MATCH (a:ACCOUNT) SHOW DATABASES YIELD name RETURN a",
        "SHOW DATABASES YIELD name CALL db.labels() YIELD label RETURN label",
        "CALL db.labels() YIELD label SHOW DATABASES YIELD name RETURN name",
        "SHOW DATABASES YIELD name SHOW DATABASES YIELD name AS n RETURN n",
        "SHOW DATABASES YIELD name CALL dbms.components() YIELD versions RETURN versions",
    ] {
        c.send(
            0x10,
            vec![string(query), map(&[]), map(&[("db", string("system"))])],
        );
        let result = metadata(c.recv(), 0x7f);
        assert!(
            matches!(&result["code"], Value::String(code) if code == FORBIDDEN || code == "Neo.ClientError.Statement.SyntaxError"),
            "{query}: {result:?}"
        );
        c.reset();
    }
    c.send(
        0x10,
        vec![
            string("SHOW DATABASES YIELD name MATCH (a:ACCOUNT) RETURN a.password_hash"),
            map(&[]),
            map(&[("db", string("system"))]),
        ],
    );
    c.failure(FORBIDDEN);
    c.reset();
    assert_eq!(
        show(&mut c, "system", "SHOW DATABASES YIELD name").1,
        vec![vec![string("varrow")]]
    );
}

#[test]
fn nonadmin_show_on_system_does_not_open_the_system_world() {
    let server = account_server();
    let mut c = alice(&server);
    // Authentication has finished. A metadata-only RUN must use the captured
    // identity and registry names even if the system file becomes unreadable.
    std::fs::write(server.dir.path().join("system.kala"), b"not a Kala log").unwrap();
    assert_eq!(
        show(&mut c, "system", "SHOW DATABASES YIELD name").1,
        vec![vec![string("varrow")]]
    );
}

#[test]
fn path_indices_agree_with_stored_relationship_direction_both_ways() {
    let server = Running::start();
    let mut c = Client::ready(&server);
    let rows = c.query("MATCH p = (a)-[r:ally_of]-(b) AT TICK -208 RETURN p, a, b, r");
    assert_eq!(rows.len(), 30);
    let mut signs: BTreeMap<i64, Vec<i64>> = BTreeMap::new();
    for row in rows {
        let path = structure(row[0].clone(), 0x50);
        assert_eq!(path.len(), 3);
        let a = structure(row[1].clone(), 0x4e);
        let b = structure(row[2].clone(), 0x4e);
        let rel = structure(row[3].clone(), 0x52);
        let Value::List(nodes) = &path[0] else {
            panic!("nodes")
        };
        assert_eq!(nodes, &vec![row[1].clone(), row[2].clone()]);
        let Value::List(rels) = &path[1] else {
            panic!("rels")
        };
        assert_eq!(rels.len(), 1);
        assert_eq!(
            structure(rels[0].clone(), 0x72),
            vec![
                rel[0].clone(),
                rel[3].clone(),
                rel[4].clone(),
                rel[5].clone()
            ]
        );
        let sign = if a[0] == rel[1] {
            assert_eq!(b[0], rel[2]);
            1
        } else {
            assert_eq!(a[0], rel[2]);
            assert_eq!(b[0], rel[1]);
            -1
        };
        assert_eq!(
            path[2],
            Value::List(vec![Value::Integer(sign), Value::Integer(1)])
        );
        let Value::Integer(id) = rel[0] else {
            panic!("id")
        };
        signs.entry(id).or_default().push(sign);
    }
    assert_eq!(signs.len(), 15);
    for signs in signs.values_mut() {
        signs.sort();
        assert_eq!(*signs, vec![-1, 1]);
    }
    let rows =
        c.query("MATCH p = (e {kala_handle: 'edwin'})-[r:present]->(c) AT TICK -240 RETURN p");
    let path = structure(rows[0][0].clone(), 0x50);
    let Value::List(rels) = &path[1] else {
        panic!("rels")
    };
    assert_eq!(
        structure(rels[0].clone(), 0x72)[2],
        map(&[("disguised", Value::Bool(true))])
    );
}

#[test]
fn every_new_secret_expression_position_is_forbidden_before_evaluation() {
    let server = account_server();
    let mut c = Client::ready(&server);
    let mut queries = vec![
        "MATCH (a:ACCOUNT) WITH a.password_hash AS h RETURN h".to_string(),
        "MATCH (a:ACCOUNT) WITH a AS b RETURN b.password_hash".into(),
        "OPTIONAL MATCH (a:ACCOUNT {password_hash: 'probe'}) RETURN a".into(),
        "UNWIND $xs AS x MATCH (a:ACCOUNT) WHERE a.password_hash = x RETURN a".into(),
        "MATCH (a:ACCOUNT) RETURN a.password_hash, count(*)".into(),
        "MATCH (a:ACCOUNT) WITH a ORDER BY a.password_hash RETURN a".into(),
        "MATCH (a:ACCOUNT)-[:missing*0..1 {password_hash: 'probe'}]-(b) RETURN a".into(),
        "CALL db.labels() YIELD label AS a WHERE a.password_hash = 'probe' RETURN a".into(),
    ];
    for function in ["count", "collect", "min", "max", "sum", "avg"] {
        queries.push(format!(
            "MATCH (a:ACCOUNT) RETURN {function}(a.password_hash)"
        ));
        queries.push(format!(
            "MATCH (a:ACCOUNT {{kala_handle: 'nobody'}}) RETURN {function}(a.password_hash) LIMIT 0"
        ));
    }
    for function in ["count", "collect"] {
        queries.push(format!(
            "MATCH (a:ACCOUNT) RETURN {function}(DISTINCT a.password_hash)"
        ));
    }
    for query in queries {
        c.send(
            0x10,
            vec![
                string(&query),
                map(&[("xs", Value::List(vec![string("probe")]))]),
                map(&[("db", string("system"))]),
            ],
        );
        let response = c.recv();
        let meta = metadata(response, 0x7f);
        assert_eq!(meta["code"], string(FORBIDDEN), "{query}: {meta:?}");
        c.reset();
    }
}

#[test]
fn account_nodes_inside_paths_are_redacted_and_property_names_are_safe() {
    let server = account_server();
    let mut c = Client::ready(&server);
    c.run(
        "MATCH p = (a:ACCOUNT) RETURN p, a",
        map(&[]),
        map(&[("db", string("system"))]),
    );
    let rows = c.pull(-1).0;
    assert!(!rows.is_empty());
    for row in rows {
        let p = structure(row[0].clone(), 0x50);
        assert_eq!(p[0], Value::List(vec![row[1].clone()]));
        assert_eq!(p[1], Value::List(vec![]));
        assert_eq!(p[2], Value::List(vec![]));
        let node = structure(row[1].clone(), 0x4e);
        let Value::Map(props) = &node[2] else {
            panic!("props")
        };
        assert!(
            !props.contains_key("password_hash")
                || props.get("password_hash") == Some(&string("<redacted>")),
            "{props:?}"
        );
    }
    c.run(
        "CALL db.propertyKeys()",
        map(&[]),
        map(&[("db", string("system"))]),
    );
    assert!(c.pull(-1).0.contains(&vec![string("password_hash")]));
}
