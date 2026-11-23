//! Edge cases of the `password_hash` refusal on `system`: mixed errors, syntax
//! errors, and attribute names that resolve to the secret (inverse names).
#[path = "common/client.rs"]
mod client;
mod common;

use client::{Client, map, string};
use common::Running;
use kala_bolt::packstream::Value;
use kala_bolt::{Server, ServerConfig, auth::Accounts, system};
use std::sync::Arc;

const FORBIDDEN: &str = "Neo.ClientError.Security.Forbidden";

fn server_with(extra: &[(&str, &str)]) -> Running {
    let dir = tempfile::tempdir().unwrap();
    drop(kala_core::load_fixture_to(common::VARROW, &dir.path().join("varrow.kala")).unwrap());
    for (name, yaml) in extra {
        drop(kala_core::load_fixture_to(yaml, &dir.path().join(name)).unwrap());
    }
    system::add(
        dir.path(),
        "carol@127.0.0.1",
        Some("test-only-secret-password"),
    )
    .unwrap();
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

fn sys(q: &str) -> String {
    format!("// kala:datalog\n{q}")
}

/// RUN must answer FAILURE directly (so no RECORD can follow); returns its code.
fn failure_code(c: &mut Client, db: &str, text: &str, params: Value) -> String {
    c.send(0x10, vec![string(text), params, on(db)]);
    match c.recv() {
        Value::Structure(0x7f, f) => {
            let Some(Value::Map(meta)) = f.first() else {
                panic!("malformed FAILURE")
            };
            let code = match meta.get("code") {
                Some(Value::String(s)) => s.clone(),
                other => panic!("no code: {other:?}"),
            };
            c.reset();
            code
        }
        Value::Structure(0x70, _) => {
            let (rows, _) = c.pull(-1);
            panic!("not refused, {} rows returned: {text}", rows.len())
        }
        other => panic!("unexpected {other:?}: {text}"),
    }
}

#[test]
fn secret_with_a_type_error_elsewhere_is_forbidden_not_a_type_error() {
    let s = server_with(&[]);
    let mut c = Client::ready(&s);
    for q in [
        // type error: string compared with integer
        "[:find ?p :where [?e :password_hash ?p] [(< ?p 5)]]",
        // type error: wrong value type for another attribute
        "[:find ?p :where [?e :password_hash ?p] [?e :user 7]]",
        // unknown attribute elsewhere
        "[:find ?p :where [?e :no_such_attribute ?x] [?e :password_hash ?p]]",
        // unknown attribute before, secret in an unused rule
        "[:find ?e :rules [[(r ?e) [?e :password_hash ?p]]] :where [?e :no_such_attribute ?x]]",
        // unbound variable elsewhere
        "[:find ?z :where [?e :password_hash ?p]]",
        // unknown rule called
        "[:find ?e :where [?e :password_hash ?p] (nosuchrule ?e)]",
    ] {
        let code = failure_code(&mut c, "system", &sys(q), map(&[]));
        assert_eq!(code, FORBIDDEN, "query: {q}");
    }
}

#[test]
fn secret_with_a_syntax_error_never_returns_rows() {
    let s = server_with(&[]);
    let mut c = Client::ready(&s);
    for q in [
        "[:find ?p :where [?e :password_hash ?p",
        "[:find ?p :where [?e :password_hash ?p]]]]",
        "[:find ?p :where [?e :password_hash]]",
        "[:find ?p :where [?e :password_hash ?p] (not]",
        "[:find ?p :where [?e :password_hash \"unterminated]]",
        ":find ?p :where [?e :password_hash ?p]",
    ] {
        // Only FAILURE-and-no-RECORD is pinned, not which code.
        let _ = failure_code(&mut c, "system", &sys(q), map(&[]));
    }
}

/// Inverse names exist only on `ref` attributes and `password_hash` is a
/// string, so no alias can resolve to it on the real system world. A copy of
/// the marker under another database name is refused by the server on load
/// (so the inverse-name branch of `is_secret_attribute` is unreachable over
/// Bolt); pin that every query on such a copy fails and returns no rows.
#[test]
fn marker_copy_with_a_ref_password_hash_inverse_is_not_queryable() {
    const COPY: &str = r#"
schema:
  node_types: [_KALA_SYSTEM, ACCOUNT]
  attributes:
    - { name: user, type: string }
    - { name: password_hash, type: ref, cardinality: many, inverse: hash_of }
entities:
  ACCOUNT: [a]
transactions:
  - ops:
      - { assert: [a, user, "x"], tick: 0 }
      - { assert: [a, password_hash, a], tick: 0 }
"#;
    let s = server_with(&[("syscopy.kala", COPY)]);
    let mut c = Client::ready(&s);
    for q in [
        "[:find ?x :where [?e :hash_of ?x]]",
        "[:find ?x :where [?e :password_hash ?x]]",
        "[:find ?x :where (not [?e :hash_of ?x])]",
    ] {
        let _ = failure_code(&mut c, "syscopy", &sys(q), map(&[]));
    }
}

#[test]
fn a_string_attribute_cannot_declare_an_inverse_so_no_alias_exists_on_real_system() {
    const BAD: &str = r#"
schema:
  node_types: [_KALA_SYSTEM]
  attributes:
    - { name: password_hash, type: string, inverse: hash_of }
"#;
    let dir = tempfile::tempdir().unwrap();
    assert!(kala_core::load_fixture_to(BAD, &dir.path().join("bad.kala")).is_err());
}
