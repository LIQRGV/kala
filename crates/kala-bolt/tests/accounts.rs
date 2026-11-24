#[path = "common/client.rs"]
mod client;
mod common;

use client::{ALLIES, Client, map, string};
use common::{Running, VARROW, assert_closed};
use kala_bolt::auth::{Accounts, AuthToken, Authenticator, Credentials};
use kala_bolt::{Server, ServerConfig};
use kala_system::system;
use std::sync::Arc;

const PASSWORD: &str = "test-only-account-password";
const UNAUTHORIZED: &str = "Neo.ClientError.Security.Unauthorized";
const FORBIDDEN: &str = "Neo.ClientError.Security.Forbidden";

fn account_server() -> Running {
    let dir = tempfile::tempdir().unwrap();
    drop(kala_fixture::load_fixture_to(VARROW, &dir.path().join("varrow.kala")).unwrap());
    system::add(dir.path(), "alice@127.0.0.1", Some(PASSWORD)).unwrap();
    system::grant(dir.path(), "alice@127.0.0.1", "read", "varrow").unwrap();
    let config = ServerConfig {
        listen: "127.0.0.1:0".parse().unwrap(),
        ..ServerConfig::default()
    };
    let auth = Arc::new(Accounts::open(dir.path()).unwrap());
    let server = Server::bind_with_auth(dir.path(), config, auth).unwrap();
    Running::from_server(dir, server)
}

fn login(server: &Running, user: &str, password: &str) -> Client {
    let mut client = Client::connected(server, 4);
    client.hello();
    client.success();
    client.send(
        0x6a,
        vec![map(&[
            ("scheme", string("basic")),
            ("principal", string(user)),
            ("credentials", string(password)),
        ])],
    );
    client
}

#[test]
fn password_account_logs_in_and_wrong_password_is_refused() {
    let server = account_server();
    let mut valid = login(&server, "alice", PASSWORD);
    valid.success();
    assert_eq!(valid.query(ALLIES).len(), 2);
    let mut wrong = login(&server, "alice", "incorrect-test-password");
    let failure = wrong.failure(UNAUTHORIZED);
    assert_eq!(
        failure.get("message"),
        Some(&string("authentication failed"))
    );
    assert_closed(&mut wrong.stream);
}

#[test]
fn grants_apply_to_every_run_and_system_requires_admin() {
    let server = account_server();
    let mut alice = login(&server, "alice", PASSWORD);
    alice.success();
    for database in ["system", "ungranted"] {
        alice.send(
            0x10,
            vec![string(ALLIES), map(&[]), map(&[("db", string(database))])],
        );
        alice.failure(FORBIDDEN);
        alice.reset();
    }
    assert_eq!(alice.query(ALLIES).len(), 2);
    let mut root = Client::ready(&server);
    root.run(
        "// kala:datalog\n[:find ?u :where [?e :user ?u]]",
        map(&[]),
        map(&[("db", string("system"))]),
    );
    assert!(
        root.pull(-1)
            .0
            .iter()
            .any(|row| row.contains(&string("root")))
    );
}

#[test]
fn revocation_is_seen_at_next_login_and_logoff_changes_identity() {
    let server = account_server();
    let mut alice = login(&server, "alice", PASSWORD);
    alice.success();
    system::revoke(server.dir.path(), "alice@127.0.0.1", "read", "varrow").unwrap();
    // Existing identity remains valid until LOGOFF, even after RESET.
    alice.reset();
    assert_eq!(alice.query(ALLIES).len(), 2);
    let mut fresh = login(&server, "alice", PASSWORD);
    fresh.success();
    fresh.send(0x10, vec![string(ALLIES), map(&[]), map(&[])]);
    fresh.failure(FORBIDDEN);
    alice.send(0x6b, vec![]);
    alice.success();
    alice.send(0x6a, vec![map(&[("scheme", string("none"))])]);
    alice.success();
    assert_eq!(alice.query(ALLIES).len(), 2);
    alice.send(0x6b, vec![]);
    alice.success();
    alice.send(
        0x6a,
        vec![map(&[
            ("scheme", string("basic")),
            ("principal", string("alice")),
            ("credentials", string(PASSWORD)),
        ])],
    );
    alice.success();
    alice.send(0x10, vec![string(ALLIES), map(&[]), map(&[])]);
    alice.failure(FORBIDDEN);
}

#[test]
fn none_maps_to_root_and_password_switch_refuses_none() {
    let server = account_server();
    assert_eq!(Client::ready(&server).query(ALLIES).len(), 2);
    system::set(server.dir.path(), "root@localhost", Some(PASSWORD)).unwrap();
    let mut none = Client::connected(&server, 4);
    none.hello();
    none.success();
    none.send(0x6a, vec![map(&[("scheme", string("none"))])]);
    none.failure(UNAUTHORIZED);
    assert_closed(&mut none.stream);
    let mut root = login(&server, "root", PASSWORD);
    root.success();
    assert_eq!(root.query(ALLIES).len(), 2);
}

fn token(scheme: &str, user: &str, password: &str) -> AuthToken {
    AuthToken {
        scheme: scheme.into(),
        principal: user.into(),
        credentials: Credentials::new(password.into()),
    }
}

#[test]
fn trust_never_authenticates_remote_peers_or_unknown_schemes() {
    let dir = tempfile::tempdir().unwrap();
    let auth = Accounts::open(dir.path()).unwrap();
    for remote in ["10.0.0.1", "::ffff:10.0.0.1", "fd00::1"] {
        assert!(
            auth.authenticate(&token("none", "", ""), remote.parse().unwrap())
                .is_err()
        );
    }
    for local in ["127.0.0.1", "::1", "::ffff:127.0.0.1"] {
        let identity = auth
            .authenticate(&token("none", "", ""), local.parse().unwrap())
            .unwrap();
        assert_eq!(identity.principal, "root");
        assert!(
            auth.authenticate(&token("unsupported", "root", ""), local.parse().unwrap())
                .is_err()
        );
    }
    assert!(system::add(dir.path(), "bob@%", None).is_err());
    system::add(dir.path(), "bob@10.0.0.0/8", Some(PASSWORD)).unwrap();
    assert!(system::set(dir.path(), "bob@10.0.0.0/8", None).is_err());
}

#[test]
fn most_specific_account_is_selected_before_password_verification() {
    let dir = tempfile::tempdir().unwrap();
    system::add(dir.path(), "alice@%", Some("broad-test-password")).unwrap();
    system::add(dir.path(), "alice@127.0.0.1", Some(PASSWORD)).unwrap();
    let auth = Accounts::open(dir.path()).unwrap();
    let peer = "::ffff:127.0.0.1".parse().unwrap();
    assert!(
        auth.authenticate(&token("basic", "alice", "broad-test-password"), peer)
            .is_err()
    );
    let identity = auth
        .authenticate(&token("basic", "alice", PASSWORD), peer)
        .unwrap();
    assert_eq!(identity.host, "127.0.0.1");
}

#[test]
fn system_queries_redact_hash_scalars_and_account_node_properties() {
    let server = account_server();
    let mut root = Client::ready(&server);
    // Reading the hash attribute is refused outright.
    root.send(
        0x10,
        vec![
            string("// kala:datalog\n[:find ?p :where [?e :password_hash ?p]]"),
            map(&[]),
            map(&[("db", string("system"))]),
        ],
    );
    root.failure("Neo.ClientError.Security.Forbidden");
    root.reset();
    let query = "[:find ?e :where [?e :user \"alice\"]]";
    root.run(
        &format!("// kala:datalog\n{query}"),
        map(&[]),
        map(&[("db", string("system"))]),
    );
    let rows = root.pull(-1).0;
    assert!(!rows.is_empty());
    let rendered = format!("{rows:?}");
    assert!(!rendered.contains("$argon2"), "hash exposed in Bolt result");
    assert!(
        !rendered.contains(PASSWORD),
        "password exposed in Bolt result"
    );
}

#[test]
fn unknown_user_runs_one_dummy_password_verification() {
    let dir = tempfile::tempdir().unwrap();
    let auth = Accounts::open(dir.path()).unwrap();
    let before = auth.verification_stats();
    let result = auth.authenticate(
        &token("basic", "unknown", PASSWORD),
        "127.0.0.1".parse().unwrap(),
    );
    let after = auth.verification_stats();
    assert_eq!(after.dummy - before.dummy, 1);
    assert_eq!(after.total - before.total, 1);
    assert!(result.is_err());
    assert_eq!(after.active, 0);
}

#[test]
fn concurrent_password_verifications_are_bounded_to_four() {
    let dir = tempfile::tempdir().unwrap();
    system::add(dir.path(), "alice@127.0.0.1", Some(PASSWORD)).unwrap();
    let auth = Arc::new(Accounts::open(dir.path()).unwrap());
    let barrier = Arc::new(std::sync::Barrier::new(9));
    let threads: Vec<_> = (0..8)
        .map(|_| {
            let auth = Arc::clone(&auth);
            let barrier = Arc::clone(&barrier);
            std::thread::spawn(move || {
                barrier.wait();
                auth.authenticate(
                    &token("basic", "alice", PASSWORD),
                    "127.0.0.1".parse().unwrap(),
                )
                .unwrap();
            })
        })
        .collect();
    barrier.wait();
    for thread in threads {
        thread.join().unwrap();
    }
    let stats = auth.verification_stats();
    assert_eq!(stats.total, 8);
    assert!((1..=4).contains(&stats.peak));
    assert_eq!(stats.active, 0);
}

#[test]
fn bootstrap_is_marked_private_and_account_changes_keep_history() {
    let dir = tempfile::tempdir().unwrap();
    assert!(system::ensure(dir.path()).unwrap());
    assert!(!system::ensure(dir.path()).unwrap());
    let path = dir.path().join("system.kala");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
    let bootstrap = std::fs::read(&path).unwrap();
    let summary = system::list(dir.path()).unwrap();
    assert_eq!(summary.len(), 1);
    assert_eq!(summary[0].user, "root");
    system::set(dir.path(), "root@localhost", Some(PASSWORD)).unwrap();
    system::set(dir.path(), "root@localhost", None).unwrap();
    let changed = std::fs::read(&path).unwrap();
    assert!(
        changed.starts_with(&bootstrap),
        "account updates must append to the audit log"
    );
    let world = kala_core::World::open_read_only(&path).unwrap();
    assert!(system::is_system(&world));
    let root = world.entities_of_type("ACCOUNT").unwrap().next().unwrap();
    let hashes = world.history(root, "password_hash").unwrap();
    assert!(hashes.iter().any(|point| point.op == kala_core::Op::Assert));
    assert!(
        hashes
            .iter()
            .any(|point| point.op == kala_core::Op::Retract)
    );
    assert!(
        world
            .as_of(root, "password_hash", world.latest_tick())
            .unwrap()
            .is_empty()
    );
}

#[test]
fn system_cannot_be_read_through_a_user_database_alias() {
    let dir = tempfile::tempdir().unwrap();
    drop(kala_fixture::load_fixture_to(VARROW, &dir.path().join("varrow.kala")).unwrap());
    system::add(dir.path(), "alice@127.0.0.1", Some(PASSWORD)).unwrap();
    system::grant(dir.path(), "alice@127.0.0.1", "read", "renamed").unwrap();
    std::fs::copy(
        dir.path().join("system.kala"),
        dir.path().join("renamed.kala"),
    )
    .unwrap();
    let auth = Arc::new(Accounts::open(dir.path()).unwrap());
    let server = Server::bind_with_auth(
        dir.path(),
        ServerConfig {
            listen: "127.0.0.1:0".parse().unwrap(),
            default_db: Some("varrow".into()),
            ..ServerConfig::default()
        },
        auth,
    )
    .unwrap();
    let server = Running::from_server(dir, server);
    let mut alice = login(&server, "alice", PASSWORD);
    alice.success();
    alice.send(
        0x10,
        vec![
            string("// kala:datalog\n[:find ?p :where [?e :password_hash ?p]]"),
            map(&[]),
            map(&[("db", string("renamed"))]),
        ],
    );
    let failure = alice.failure("Neo.DatabaseError.General.UnknownError");
    assert!(!format!("{failure:?}").contains("$argon2"));
}

#[test]
fn existing_user_world_cannot_occupy_reserved_system_name() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("system.kala");
    drop(kala_fixture::load_fixture_to(VARROW, &path).unwrap());
    let original = std::fs::read(&path).unwrap();
    assert!(system::ensure(dir.path()).is_err());
    assert_eq!(std::fs::read(path).unwrap(), original);
}

#[test]
fn concurrent_last_admin_removals_leave_an_administrator() {
    let dir = tempfile::tempdir().unwrap();
    system::add(dir.path(), "backup@localhost", None).unwrap();
    system::grant(dir.path(), "backup@localhost", "admin", "*").unwrap();
    let barrier = Arc::new(std::sync::Barrier::new(3));
    let workers: Vec<_> = ["root@localhost", "backup@localhost"]
        .into_iter()
        .map(|account| {
            let path = dir.path().to_owned();
            let barrier = Arc::clone(&barrier);
            std::thread::spawn(move || {
                barrier.wait();
                system::drop_account(path, account)
            })
        })
        .collect();
    barrier.wait();
    let outcomes: Vec<_> = workers
        .into_iter()
        .map(|worker| worker.join().unwrap())
        .collect();
    assert_eq!(outcomes.iter().filter(|result| result.is_ok()).count(), 1);
    assert_eq!(
        system::list(dir.path())
            .unwrap()
            .iter()
            .filter(|account| account.grants.iter().any(|grant| grant == "admin:*"))
            .count(),
        1
    );
}

#[test]
#[cfg(unix)]
fn recovering_system_tail_keeps_quarantined_bytes_private() {
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    system::ensure(dir.path()).unwrap();
    let path = dir.path().join("system.kala");
    let committed = std::fs::read(&path).unwrap();
    let tail = b"$argon2id$partial";
    std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap()
        .write_all(tail)
        .unwrap();
    system::grant(dir.path(), "root@localhost", "read", "varrow").unwrap();
    let quarantined: Vec<_> = std::fs::read_dir(dir.path())
        .unwrap()
        .map(Result::unwrap)
        .filter(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .starts_with("system.kala.torn-")
        })
        .collect();
    assert_eq!(quarantined.len(), 1);
    let quarantine = quarantined[0].path();
    assert_eq!(
        std::fs::metadata(&quarantine).unwrap().permissions().mode() & 0o777,
        0o600,
        "quarantine must retain the system world's private permissions"
    );
    assert!(
        std::fs::read(quarantine).unwrap() == tail,
        "quarantine must preserve exact torn bytes"
    );
    assert!(std::fs::read(path).unwrap().starts_with(&committed));
}

#[test]
fn driver_empty_auth_map_is_none_for_hello_and_logon() {
    let server = account_server();
    for minor in [0, 4] {
        let mut client = Client::connected(&server, minor);
        client.hello();
        client.success();
        if minor >= 1 {
            client.send(0x6a, vec![map(&[])]);
            client.success();
        }
        assert_eq!(client.query(ALLIES).len(), 2);
    }
    system::set(server.dir.path(), "root@localhost", Some(PASSWORD)).unwrap();
    let mut client = Client::connected(&server, 4);
    client.hello();
    client.success();
    client.send(0x6a, vec![map(&[])]);
    client.failure(UNAUTHORIZED);
    assert_closed(&mut client.stream);
}

#[test]
fn malformed_auth_does_not_fall_back_to_none() {
    let server = account_server();
    for auth in [
        map(&[("scheme", string(""))]),
        map(&[("principal", string("root"))]),
        map(&[("credentials", string(PASSWORD))]),
        map(&[
            ("principal", string("alice")),
            ("credentials", string(PASSWORD)),
        ]),
    ] {
        let mut client = Client::connected(&server, 4);
        client.hello();
        client.success();
        client.send(0x6a, vec![auth]);
        client.failure(UNAUTHORIZED);
        assert_closed(&mut client.stream);
    }
}

#[test]
fn backup_only_grant_never_authorizes_bolt_read() {
    let dir = tempfile::tempdir().unwrap();
    drop(kala_fixture::load_fixture_to(VARROW, &dir.path().join("varrow.kala")).unwrap());
    system::add(dir.path(), "alice@127.0.0.1", Some(PASSWORD)).unwrap();
    system::grant(dir.path(), "alice@127.0.0.1", "backup", "varrow").unwrap();
    let auth = Arc::new(Accounts::open(dir.path()).unwrap());
    let server = Server::bind_with_auth(
        dir.path(),
        ServerConfig {
            listen: "127.0.0.1:0".parse().unwrap(),
            ..ServerConfig::default()
        },
        auth,
    )
    .unwrap();
    let server = Running::from_server(dir, server);
    let mut alice = login(&server, "alice", PASSWORD);
    alice.success();
    alice.send(
        0x10,
        vec![string(ALLIES), map(&[]), map(&[("db", string("varrow"))])],
    );
    alice.failure(FORBIDDEN);
}

#[test]
fn backup_grants_never_cover_the_accounts_database() {
    let id = |grants: &[&str]| kala_bolt::auth::Identity {
        principal: "bob".into(),
        host: "%".into(),
        grants: grants.iter().map(|g| (*g).to_owned()).collect(),
    };
    let wildcard = id(&["backup:*"]);
    assert!(wildcard.permits_backup("varrow"));
    assert!(
        !wildcard.permits_backup("system"),
        "backup:* must not copy hashes"
    );
    assert!(!id(&["backup:system"]).permits_backup("system"));
    assert!(id(&["admin:*"]).permits_backup("system"));
    assert!(id(&["backup:varrow"]).permits_backup("varrow"));
    assert!(!id(&["backup:varrow"]).permits_backup("other"));
}
