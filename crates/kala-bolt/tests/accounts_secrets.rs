//! Password hashes must not enter query evaluation over Bolt on `system`.
//!
//! Output redaction alone lets an admin learn a hash one prefix at a time with
//! comparisons (`<`, `=`) or `min`/`max`. A query on `system` whose plan reads
//! `password_hash` is refused at RUN with `Forbidden`, before any RECORD.
//! Datalog's attribute position is always a keyword (parse.rs), so there is no
//! variable-attribute form today; a test pins that it stays unreachable.
#[path = "common/client.rs"]
mod client;
mod common;

use client::{Client, map, string};
use common::Running;
use kala_bolt::packstream::Value;
use kala_bolt::{Server, ServerConfig, auth::Accounts, system};
use std::sync::Arc;

const PASSWORD: &str = "test-only-secret-password";
const FORBIDDEN: &str = "Neo.ClientError.Security.Forbidden";

const USERS_YAML: &str = r#"
schema:
  node_types: [CHARACTER]
  attributes:
    - { name: name, type: string }
    - { name: password_hash, type: string, indexed: false }
entities:
  CHARACTER: [bob]
transactions:
  - ops:
      - { assert: [bob, name, "Bob"], tick: 0 }
      - { assert: [bob, password_hash, "$argon2id$user-data"], tick: 0 }
"#;

fn server() -> Running {
    let dir = tempfile::tempdir().unwrap();
    drop(kala_core::load_fixture_to(common::VARROW, &dir.path().join("varrow.kala")).unwrap());
    drop(kala_core::load_fixture_to(USERS_YAML, &dir.path().join("users.kala")).unwrap());
    system::add(dir.path(), "carol@127.0.0.1", Some(PASSWORD)).unwrap();
    system::grant(dir.path(), "carol@127.0.0.1", "read", "varrow").unwrap();
    let config = ServerConfig {
        listen: "127.0.0.1:0".parse().unwrap(),
        ..ServerConfig::default()
    };
    let auth = Arc::new(Accounts::open(dir.path()).unwrap());
    let server = Server::bind_with_auth(dir.path(), config, auth).unwrap();
    Running::from_server(dir, server)
}

fn on(db: &str) -> Value {
    map(&[("db", string(db))])
}

/// RUN `text` on `db`; the very next message must be FAILURE Forbidden. If the
/// server answers SUCCESS instead, pull and report how many rows leaked.
fn assert_forbidden(client: &mut Client, db: &str, text: &str, params: Value) {
    client.send(0x10, vec![string(text), params, on(db)]);
    let first = client.recv();
    match first {
        Value::Structure(0x7f, fields) => {
            let Some(Value::Map(meta)) = fields.first() else {
                panic!("malformed FAILURE")
            };
            assert_eq!(
                meta.get("code"),
                Some(&string(FORBIDDEN)),
                "query: {text}\nwrong failure: {meta:?}"
            );
            client.reset();
        }
        Value::Structure(0x70, _) => {
            let (rows, _) = client.pull(-1);
            panic!(
                "query was NOT refused (RUN succeeded, PULL returned {} rows): {text}",
                rows.len()
            );
        }
        other => panic!("unexpected reply {other:?} for {text}"),
    }
}

fn rows(client: &mut Client, db: &str, text: &str) -> Vec<Vec<Value>> {
    client.run(text, map(&[]), on(db));
    client.pull(-1).0
}

fn sys(query: &str) -> String {
    format!("// kala:datalog\n{query}")
}

// ---- the attack -------------------------------------------------------------

#[test]
fn comparison_less_than_on_password_hash_is_forbidden() {
    let server = server();
    let mut root = Client::ready(&server);
    // Hashes start with `$argon2id$`; 'i' < 'z', so today this leaks a bit per query.
    assert_forbidden(
        &mut root,
        "system",
        &sys("[:find ?u :where [?e :user ?u] [?e :password_hash ?p] [(< ?p \"$argon2z\")]]"),
        map(&[]),
    );
}

#[test]
fn comparison_greater_than_and_ge_le_on_password_hash_are_forbidden() {
    let server = server();
    let mut root = Client::ready(&server);
    for op in [">", ">=", "<=", "!="] {
        assert_forbidden(
            &mut root,
            "system",
            &sys(&format!(
                "[:find ?u :where [?e :user ?u] [?e :password_hash ?p] [({op} ?p \"$argon2\")]]"
            )),
            map(&[]),
        );
    }
}

#[test]
fn equality_on_password_hash_is_forbidden() {
    let server = server();
    let mut root = Client::ready(&server);
    assert_forbidden(
        &mut root,
        "system",
        &sys("[:find ?u :where [?e :user ?u] [?e :password_hash ?p] [(= ?p \"$argon2id$guess\")]]"),
        map(&[]),
    );
    // Equality against the hash as a constant in the value position.
    assert_forbidden(
        &mut root,
        "system",
        &sys("[:find ?u :where [?e :user ?u] [?e :password_hash \"$argon2id$guess\"]]"),
        map(&[]),
    );
}

#[test]
fn comparison_against_a_parameter_on_password_hash_is_forbidden() {
    let server = server();
    let mut root = Client::ready(&server);
    assert_forbidden(
        &mut root,
        "system",
        &sys("[:find ?u :in $ ?g :where [?e :user ?u] [?e :password_hash ?p] [(< ?p ?g)]]"),
        map(&[("g", string("$argon2z"))]),
    );
}

#[test]
fn min_and_max_aggregates_on_password_hash_are_forbidden() {
    let server = server();
    let mut root = Client::ready(&server);
    for agg in ["min", "max", "count-distinct", "count"] {
        assert_forbidden(
            &mut root,
            "system",
            &sys(&format!("[:find ({agg} ?p) :where [?e :password_hash ?p]]")),
            map(&[]),
        );
    }
}

#[test]
fn plain_read_of_password_hash_is_forbidden() {
    let server = server();
    let mut root = Client::ready(&server);
    assert_forbidden(
        &mut root,
        "system",
        &sys("[:find ?p :where [?e :password_hash ?p]]"),
        map(&[]),
    );
    // Even when the value is not returned.
    assert_forbidden(
        &mut root,
        "system",
        &sys("[:find ?e :where [?e :password_hash _]]"),
        map(&[]),
    );
}

// ---- every syntactic place a pattern can sit -------------------------------

#[test]
fn history_pattern_on_password_hash_is_forbidden() {
    let server = server();
    let mut root = Client::ready(&server);
    for pattern in [
        "[?e :password_hash ?p ?t]",
        "[?e :password_hash ?p ?t ?op]",
        "[?e :password_hash ?p ?t :assert]",
    ] {
        assert_forbidden(
            &mut root,
            "system",
            &sys(&format!(
                "[:find ?u :where [?e :user ?u] {pattern} [(< ?p \"$argon2z\")]]"
            )),
            map(&[]),
        );
    }
}

#[test]
fn during_on_password_hash_is_forbidden() {
    let server = server();
    let mut root = Client::ready(&server);
    assert_forbidden(
        &mut root,
        "system",
        &sys("[:find ?s :where (during [?e :password_hash ?p] ?s)]"),
        map(&[]),
    );
}

#[test]
fn at_clause_on_password_hash_is_forbidden() {
    let server = server();
    let mut root = Client::ready(&server);
    assert_forbidden(
        &mut root,
        "system",
        &sys(
            "[:find ?u :where [?e :user ?u] (at 1000000 [?e :password_hash ?p]) [(< ?p \"$argon2z\")]]",
        ),
        map(&[]),
    );
}

#[test]
fn negation_over_password_hash_is_forbidden() {
    let server = server();
    let mut root = Client::ready(&server);
    // An oracle: rows survive only if no hash is below the probe.
    assert_forbidden(
        &mut root,
        "system",
        &sys("[:find ?u :where [?e :user ?u] (not [?e :password_hash ?p] [(< ?p \"$argon2z\")])]"),
        map(&[]),
    );
}

#[test]
fn or_branch_over_password_hash_is_forbidden() {
    let server = server();
    let mut root = Client::ready(&server);
    assert_forbidden(
        &mut root,
        "system",
        &sys("[:find ?u :where [?e :user ?u] \
             (or (and [?e :password_hash ?p] [(< ?p \"$argon2z\")]) (and [?e :password_hash ?p] [(> ?p \"$argon2z\")]))]"),
        map(&[]),
    );
}

#[test]
fn rule_body_over_password_hash_is_forbidden() {
    let server = server();
    let mut root = Client::ready(&server);
    assert_forbidden(
        &mut root,
        "system",
        &sys(
            "[:find ?u :rules [[(leaky ?e) [?e :password_hash ?p] [(< ?p \"$argon2z\")]]] \
             :where [?e :user ?u] (leaky ?e)]",
        ),
        map(&[]),
    );
}

#[test]
fn unused_rule_naming_password_hash_is_still_forbidden() {
    let server = server();
    let mut root = Client::ready(&server);
    // The refusal is about what the query names, not what happens to run.
    assert_forbidden(
        &mut root,
        "system",
        &sys("[:find ?u :rules [[(leaky ?e) [?e :password_hash ?p]]] \
             :where [?e :user ?u] (leaky ?e)]"),
        map(&[]),
    );
}

#[test]
fn older_canon_read_of_password_hash_is_forbidden() {
    let server = server();
    let mut root = Client::ready(&server);
    assert_forbidden(
        &mut root,
        "system",
        &sys(
            "[:find ?u :canon 3 :where [?e :user ?u] [?e :password_hash ?p] [(< ?p \"$argon2z\")]]",
        ),
        map(&[]),
    );
}

// ---- no way to reach an attribute without naming it ------------------------

/// Guard (passes today): the attribute position is keyword-only, so a variable
/// attribute is a parse error and cannot reach `password_hash`. If a variable
/// attribute is ever added, this must become Forbidden on `system`, so the test
/// accepts only FAILURE and never rows.
#[test]
fn guard_variable_attribute_position_never_returns_rows_on_system() {
    let server = server();
    let mut root = Client::ready(&server);
    for text in [
        "[:find ?p :where [?e ?a ?p] [(< ?p \"$argon2z\")]]",
        "[:find ?p :in $ ?a :where [?e ?a ?p]]",
        "[:find ?p :where [?e ?a ?p ?t]]",
        "[:find ?s :where (during [?e ?a ?p] ?s)]",
    ] {
        root.send(0x10, vec![string(&sys(text)), map(&[]), on("system")]);
        match root.recv() {
            Value::Structure(0x7f, _) => root.reset(),
            other => panic!("variable attribute was accepted: {text}: {other:?}"),
        }
    }
}

/// Guard: Datalog has no parameter that names an attribute either.
#[test]
fn guard_attribute_name_parameter_is_not_accepted_on_system() {
    let server = server();
    let mut root = Client::ready(&server);
    root.send(
        0x10,
        vec![
            string(&sys("[:find ?p :in $ ?a :where [?e ?a ?p]]")),
            map(&[("a", string("password_hash"))]),
            on("system"),
        ],
    );
    assert!(matches!(root.recv(), Value::Structure(0x7f, _)));
}

// ---- guards: ordinary use keeps working ------------------------------------

#[test]
fn guard_ordinary_admin_queries_on_system_still_work() {
    let server = server();
    let mut root = Client::ready(&server);
    let users = rows(&mut root, "system", &sys("[:find ?u :where [?e :user ?u]]"));
    assert!(users.iter().any(|r| r.contains(&string("root"))));
    assert!(users.iter().any(|r| r.contains(&string("carol"))));
    let hosts = rows(
        &mut root,
        "system",
        &sys("[:find ?u ?h :where [?e :user ?u] [?e :host ?h] [(= ?h \"localhost\")]]"),
    );
    assert!(hosts.iter().any(|r| r.contains(&string("root"))));
    assert!(!hosts.iter().any(|r| r.contains(&string("carol"))));
    let grants = rows(
        &mut root,
        "system",
        &sys("[:find ?g :where [?e :user \"carol\"] [?e :grant ?g]]"),
    );
    assert!(grants.iter().any(|r| r.contains(&string("read:varrow"))));
    let plugins = rows(
        &mut root,
        "system",
        &sys("[:find ?pl :where [?e :user \"carol\"] [?e :plugin ?pl]]"),
    );
    assert_eq!(plugins.len(), 1);
    // History and during on non-secret attributes are fine.
    assert!(
        !rows(
            &mut root,
            "system",
            &sys("[:find ?g ?t :where [?e :user \"carol\"] [?e :grant ?g ?t]]"),
        )
        .is_empty()
    );
}

#[test]
fn guard_refusal_is_only_on_system_not_on_a_user_database_with_that_attribute() {
    let server = server();
    let mut root = Client::ready(&server);
    let found = rows(
        &mut root,
        "users",
        &sys("[:find ?n :where [?e :name ?n] [?e :password_hash ?p] [(< ?p \"$argon2z\")]]"),
    );
    assert_eq!(found, vec![vec![string("Bob")]]);
    // A user world's own values are not redacted-by-design here; only the
    // refusal is scoped to `system`.
}

#[test]
fn guard_account_node_results_and_scalars_stay_redacted() {
    let server = server();
    let mut root = Client::ready(&server);
    let rendered = format!(
        "{:?}",
        rows(
            &mut root,
            "system",
            &sys("[:find ?e :where [?e :user \"carol\"]]")
        )
    );
    assert!(rendered.contains("<redacted>"), "{rendered}");
    assert!(!rendered.contains("$argon2"), "{rendered}");
    assert!(!rendered.contains(PASSWORD), "{rendered}");
}

#[test]
fn guard_non_admin_still_gets_forbidden_on_system_without_reading_anything() {
    let server = server();
    let mut carol = Client::connected(&server, 4);
    carol.hello();
    carol.success();
    carol.send(
        0x6a,
        vec![map(&[
            ("scheme", string("basic")),
            ("principal", string("carol")),
            ("credentials", string(PASSWORD)),
        ])],
    );
    carol.success();
    assert_forbidden(
        &mut carol,
        "system",
        &sys("[:find ?u :where [?e :user ?u]]"),
        map(&[]),
    );
}
