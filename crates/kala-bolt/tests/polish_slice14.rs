//! Slice 14 review fixes, over real sockets: impersonation on RUN, ROUTE on
//! `system`, database errors before syntax errors, and paths through edges.
#[path = "common/client.rs"]
mod client;
mod common;
use client::{Client, map, metadata, string, structure};
use common::Running;
use kala_bolt::packstream::Value;
use kala_bolt::{Server, ServerConfig};
use kala_system::system;
use std::collections::BTreeMap;

const INVALID: &str = "Neo.ClientError.Request.Invalid";
const FORBIDDEN: &str = "Neo.ClientError.Security.Forbidden";
const NOT_FOUND: &str = "Neo.ClientError.Database.DatabaseNotFound";
const EXECUTION: &str = "Neo.ClientError.Statement.ExecutionFailed";
const MALFORMED: &str = "MATCH (n RETURN n";

/// `varrow` and `hidden`, plus `alice@localhost` who may read only `varrow`.
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
        listen: vec!["127.0.0.1:0".parse().unwrap()],
        default_db: Some("varrow".into()),
        ..ServerConfig::default()
    };
    let server = Server::bind_accounts(dir.path(), config).unwrap();
    Running::from_server(dir, server)
}

fn alice(server: &Running) -> Client {
    let mut c = Client::connected(server, 4);
    c.hello();
    c.success();
    c.send(
        0x6a,
        vec![map(&[
            ("scheme", string("basic")),
            ("principal", string("alice")),
            ("credentials", string("")),
        ])],
    );
    c.success();
    c
}

fn send_run(c: &mut Client, query: &str, extra: &[(&str, Value)]) {
    c.send(0x10, vec![string(query), map(&[]), map(extra)]);
}

fn send_route(c: &mut Client, extra: &[(&str, Value)]) {
    c.send(0x66, vec![map(&[]), Value::List(vec![]), map(extra)]);
}

fn text(meta: &BTreeMap<String, Value>, key: &str) -> String {
    match &meta[key] {
        Value::String(s) => s.clone(),
        other => panic!("{key}: {other:?}"),
    }
}

/// Sends a RUN that must fail, returns its message and resets the session.
fn run_fails(c: &mut Client, query: &str, db: &str, code: &str) -> String {
    send_run(c, query, &[("db", string(db))]);
    let message = text(&c.failure(code), "message");
    c.reset();
    message
}

#[test]
fn run_refuses_impersonation_exactly_as_route_does() {
    let server = account_server();
    // root is an admin; alice may not read `hidden`. Running "as alice" with
    // root's own grants would be a silent privilege mix-up.
    let mut c = Client::ready(&server);
    send_route(&mut c, &[("imp_user", string("alice"))]);
    let route = c.failure(FORBIDDEN);
    c.reset();
    for db in ["hidden", "varrow", "system", "does-not-exist"] {
        send_run(
            &mut c,
            "MATCH (n) RETURN n LIMIT 1",
            &[("db", string(db)), ("imp_user", string("alice"))],
        );
        let run = c.failure(FORBIDDEN);
        assert_eq!(run, route, "{db}");
        assert_eq!(text(&run, "message"), "impersonation is not supported");
        // The normal failure flow: ignored until RESET, then usable again.
        send_run(&mut c, "RETURN 1", &[]);
        assert!(structure(c.recv(), 0x7e).is_empty());
        c.reset();
    }
    // Refused before the text is looked at, and for server metadata too.
    for query in [MALFORMED, "SHOW DATABASES", "CALL dbms.components()"] {
        send_run(&mut c, query, &[("imp_user", string("alice"))]);
        assert_eq!(c.failure(FORBIDDEN), route, "{query}");
        c.reset();
    }
    for bad in [
        Value::Integer(1),
        Value::Bool(true),
        Value::List(vec![]),
        map(&[]),
    ] {
        send_run(&mut c, "RETURN 1", &[("imp_user", bad.clone())]);
        c.failure(INVALID);
        c.reset();
        send_route(&mut c, &[("imp_user", bad)]);
        c.failure(INVALID);
        c.reset();
    }
    // Absent or null is no impersonation.
    send_run(&mut c, client::ALLIES, &[("imp_user", Value::Null)]);
    c.success();
    assert_eq!(c.pull(-1).0.len(), 2);
    assert_eq!(c.query(client::ALLIES).len(), 2);
}

#[test]
fn run_refuses_impersonation_without_accounts_too() {
    let server = Running::start();
    let mut c = Client::ready(&server);
    send_run(&mut c, client::ALLIES, &[("imp_user", string("alice"))]);
    let failure = c.failure(FORBIDDEN);
    assert_eq!(text(&failure, "message"), "impersonation is not supported");
    c.reset();
    assert_eq!(c.query(client::ALLIES).len(), 2);
}

#[test]
fn route_on_system_works_for_a_non_admin_but_admits_no_system_read() {
    let server = account_server();
    let mut c = alice(&server);
    send_route(&mut c, &[("db", string("system"))]);
    let meta = c.success();
    let Value::Map(rt) = &meta["rt"] else {
        panic!("routing table")
    };
    assert_eq!(rt["db"], string("system"));
    let Value::List(servers) = &rt["servers"] else {
        panic!("servers")
    };
    assert_eq!(servers.len(), 3);
    for entry in servers {
        let Value::Map(entry) = entry else {
            panic!("server")
        };
        assert_eq!(
            entry["addresses"],
            Value::List(vec![string(&server.addr.to_string())])
        );
    }
    // The routing table is not a grant: RUN still guards the system world.
    for query in [
        "MATCH (a) RETURN a",
        "MATCH (a:ACCOUNT) RETURN a.user",
        "CALL db.labels()",
        "CALL dbms.components()",
    ] {
        let message = run_fails(&mut c, query, "system", FORBIDDEN);
        assert_eq!(message, "access to database \"system\" is forbidden");
    }
    send_run(
        &mut c,
        "SHOW DATABASES YIELD name",
        &[("db", string("system"))],
    );
    c.success();
    assert_eq!(c.pull(-1).0, vec![vec![string("varrow")]]);
}

#[test]
fn route_on_system_works_without_accounts_as_show_databases_does() {
    let server = Running::start();
    let mut c = Client::ready(&server);
    send_route(&mut c, &[("db", string("system"))]);
    c.success();
    run_fails(&mut c, "MATCH (a) RETURN a", "system", FORBIDDEN);
    send_run(
        &mut c,
        "SHOW DATABASES YIELD name",
        &[("db", string("system"))],
    );
    c.success();
    assert_eq!(c.pull(-1).0, vec![vec![string("varrow")]]);
}

#[test]
fn route_keeps_run_failures_for_unknown_and_forbidden_databases() {
    let server = account_server();
    // A non-admin cannot tell a hidden database from a missing one.
    let mut user = alice(&server);
    for db in ["hidden", "does-not-exist"] {
        let expected = format!("access to database {db:?} is forbidden");
        send_route(&mut user, &[("db", string(db))]);
        assert_eq!(text(&user.failure(FORBIDDEN), "message"), expected);
        user.reset();
        let message = run_fails(&mut user, "MATCH (n) RETURN n LIMIT 0", db, FORBIDDEN);
        assert_eq!(message, expected);
    }
    // An admin (wildcard grant) is told the database does not exist.
    let mut root = Client::ready(&server);
    let expected = "database \"does-not-exist\" was not found";
    send_route(&mut root, &[("db", string("does-not-exist"))]);
    assert_eq!(text(&root.failure(NOT_FOUND), "message"), expected);
    root.reset();
    let message = run_fails(
        &mut root,
        "MATCH (n) RETURN n LIMIT 0",
        "does-not-exist",
        NOT_FOUND,
    );
    assert_eq!(message, expected);
    for db in ["hidden", "system", "varrow"] {
        send_route(&mut root, &[("db", string(db))]);
        c_db(&root.success(), db);
    }
}

fn c_db(meta: &BTreeMap<String, Value>, db: &str) {
    let Value::Map(rt) = &meta["rt"] else {
        panic!("routing table")
    };
    assert_eq!(rt["db"], string(db));
}

#[test]
fn database_errors_come_before_syntax_errors() {
    let server = account_server();
    let mut root = Client::ready(&server);
    assert_eq!(
        run_fails(&mut root, MALFORMED, "nope", NOT_FOUND),
        "database \"nope\" was not found"
    );
    // On a database that exists and may be read, the text is still parsed.
    run_fails(
        &mut root,
        MALFORMED,
        "varrow",
        "Neo.ClientError.Statement.SyntaxError",
    );
    let mut user = alice(&server);
    for db in ["hidden", "nope"] {
        assert_eq!(
            run_fails(&mut user, MALFORMED, db, FORBIDDEN),
            format!("access to database {db:?} is forbidden")
        );
    }
    // Server metadata on an unreadable ordinary database is refused as well.
    for query in ["SHOW DATABASES", "CALL dbms.components()"] {
        run_fails(&mut user, query, "hidden", FORBIDDEN);
    }
}

#[test]
fn a_non_admin_on_system_learns_only_forbidden_unless_listing_databases() {
    let server = account_server();
    let mut user = alice(&server);
    for query in [
        MALFORMED,
        "MATCH (a) RETURN a",
        "MATCH (a:ACCOUNT) RETURN a.password_hash",
        "SHOW DATABASES YIELD name WHERE name.password_hash = 'x' RETURN name",
        "SHOW DATABASES YIELD name MATCH (a:ACCOUNT) RETURN a.user",
        "SHOW DATABASES YIELD",
        "CALL db.nosuch()",
        "// kala:datalog\n[:find ?e :where [?e :user ?u]]",
        "// kala:datalog\n[:find ?e :where",
        "// kala:gremlin\ng.V()",
    ] {
        send_run(&mut user, query, &[("db", string("system"))]);
        let failure = user.failure(FORBIDDEN);
        assert_eq!(
            text(&failure, "message"),
            "access to database \"system\" is forbidden",
            "{query}"
        );
        user.reset();
    }
    // The exemption itself is unchanged: a pure database listing still runs.
    send_run(
        &mut user,
        "SHOW DATABASES YIELD name",
        &[("db", string("system"))],
    );
    user.success();
    assert_eq!(user.pull(-1).0, vec![vec![string("varrow")]]);
    // An admin on `system` still gets ordinary diagnostics and protections.
    let mut root = Client::ready(&server);
    run_fails(
        &mut root,
        MALFORMED,
        "system",
        "Neo.ClientError.Statement.SyntaxError",
    );
    let message = run_fails(
        &mut root,
        "MATCH (a:ACCOUNT) RETURN a.password_hash",
        "system",
        FORBIDDEN,
    );
    assert!(message.contains("protected attribute"), "{message}");
    let message = run_fails(
        &mut root,
        "SHOW DATABASES YIELD name WHERE name.password_hash = 'x' RETURN name",
        "system",
        FORBIDDEN,
    );
    assert!(message.contains("protected attribute"), "{message}");
}

const NOTE_WORLD: &str = "
schema:
  node_types: [CHARACTER, NOTE]
  attributes:
    - { name: ally_of, type: ref, cardinality: many }
    - { name: about, type: ref, cardinality: many }
entities:
  CHARACTER: [edwin, mara]
  NOTE: [note]
transactions:
  - tick: 1
    ops:
      - assert: [edwin, ally_of, mara]
      - assert: [note, about, [edwin, ally_of, mara]]
      - assert: [note, about, mara]
";

#[test]
fn a_path_ending_on_an_edge_entity_is_refused_not_sent_malformed() {
    let dir = tempfile::tempdir().unwrap();
    drop(kala_fixture::load_fixture_to(NOTE_WORLD, &dir.path().join("notes.kala")).unwrap());
    let server = Running::in_dir(
        dir,
        ServerConfig {
            listen: vec!["127.0.0.1:0".parse().unwrap()],
            ..ServerConfig::default()
        },
    );
    let mut c = Client::ready(&server);
    // Returning the targets themselves keeps working: one Node, one Relationship.
    let rows = c.query("MATCH (n {kala_handle:'note'})-[:about]->(x) RETURN x");
    let mut tags: Vec<u8> = rows
        .iter()
        .map(|row| match &row[0] {
            Value::Structure(tag, _) => *tag,
            other => panic!("{other:?}"),
        })
        .collect();
    tags.sort_unstable();
    assert_eq!(tags, vec![0x4e, 0x52]);
    // A Path's node list may hold only Nodes.
    send_run(
        &mut c,
        "MATCH p = (n {kala_handle:'note'})-[:about]->(x) RETURN p",
        &[],
    );
    let response = c.recv();
    if matches!(response, Value::Structure(0x70, _)) {
        let rows = c.pull(-1).0;
        panic!("a Path through an edge entity was sent: {rows:?}");
    }
    let failure = metadata(response, 0x7f);
    assert_eq!(failure["code"], string(EXECUTION));
    assert_eq!(
        text(&failure, "message"),
        "a path through an edge entity cannot be returned as a Path; return its nodes and relationships separately"
    );
    c.reset();
    // A path between ordinary nodes in the same world is unaffected.
    let rows =
        c.query("MATCH p = (n {kala_handle:'note'})-[:about]->(x {kala_handle:'mara'}) RETURN p");
    assert_eq!(rows.len(), 1);
    let path = structure(rows[0][0].clone(), 0x50);
    let Value::List(nodes) = &path[0] else {
        panic!("nodes")
    };
    assert_eq!(nodes.len(), 2);
    for node in nodes {
        structure(node.clone(), 0x4e);
    }
}
