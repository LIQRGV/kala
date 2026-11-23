//! PR #5 polish: `NoAuth` is a development mode, not an administrator, and
//! never serves the reserved `system` database.
#[path = "common/client.rs"]
mod client;
mod common;

use client::{ALLIES, Client, map, string};
use common::{Running, VARROW};
use kala_bolt::auth::{AuthToken, Authenticator, Credentials, NoAuth};
use kala_bolt::packstream::Value;
use kala_bolt::{ServerConfig, system};

#[test]
fn no_auth_identity_holds_read_star_and_not_admin() {
    for scheme in ["none", "basic", "future"] {
        let token = AuthToken {
            scheme: scheme.into(),
            principal: "anyone".into(),
            credentials: Credentials::new("x".into()),
        };
        let identity = NoAuth
            .authenticate(&token, "127.0.0.1".parse().unwrap())
            .unwrap();
        assert_eq!(identity.grants, vec!["read:*".to_string()], "{scheme}");
        assert!(identity.permits_read("varrow"));
        assert!(!identity.permits_read("system"));
    }
}

fn dir_with_real_system_world() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    drop(kala_fixture::load_fixture_to(VARROW, &dir.path().join("varrow.kala")).unwrap());
    assert!(system::ensure(dir.path()).unwrap());
    dir
}

#[test]
fn no_auth_server_never_serves_the_system_database_but_serves_the_others() {
    let dir = dir_with_real_system_world();
    let server = Running::in_dir(
        dir,
        ServerConfig {
            listen: "127.0.0.1:0".parse().unwrap(),
            ..ServerConfig::default()
        },
    );
    let mut client = Client::ready(&server);
    // The ordinary database still works.
    assert_eq!(client.query(ALLIES).len(), 2);
    // `system` is refused, by name, under NoAuth.
    client.send(
        0x10,
        vec![
            string("// kala:datalog\n[:find ?u :where [?e :user ?u]]"),
            map(&[]),
            map(&[("db", string("system"))]),
        ],
    );
    match client.recv() {
        Value::Structure(0x7f, _) => {}
        other => panic!("RUN against db system must FAIL under NoAuth, got {other:?}"),
    }
}

#[test]
fn bind_with_auth_no_auth_refuses_system_db_by_grants_even_when_present() {
    let dir = dir_with_real_system_world();
    let config = ServerConfig {
        listen: "127.0.0.1:0".parse().unwrap(),
        ..ServerConfig::default()
    };
    let server =
        kala_bolt::Server::bind_with_auth(dir.path(), config, std::sync::Arc::new(NoAuth)).unwrap();
    let running = Running::from_server(dir, server);
    let mut client = Client::ready(&running);
    assert_eq!(client.query(ALLIES).len(), 2);
    client.send(
        0x10,
        vec![
            string("// kala:datalog\n[:find ?u :where [?e :user ?u]]"),
            map(&[]),
            map(&[("db", string("system"))]),
        ],
    );
    client.failure("Neo.ClientError.Security.Forbidden");
}
