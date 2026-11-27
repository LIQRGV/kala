//! Review fixes for slice 15c (Bolt writes and explicit transactions).
#[path = "common/client.rs"]
mod client;
mod common;

use client::{Client, map, string};
use common::{Running, VARROW};
use kala_bolt::packstream::Value;
use kala_bolt::{Server, ServerConfig};
use kala_core::World;
use std::collections::BTreeMap;
use std::time::{Duration, Instant};

const FORBIDDEN: &str = "Neo.ClientError.Security.Forbidden";
const TIMED_OUT: &str = "Neo.ClientError.Transaction.TransactionTimedOut";
const BOOKMARK: &str = "Neo.ClientError.Transaction.InvalidBookmark";
const WRITE: &str = "// kala:datalog\n[:assert :greg :alias \"Ned\" :at 300]";
const ALIASES: &str = "// kala:datalog\n[:find ?a :at 300 :where [:greg :alias ?a]]";

type Meta = BTreeMap<String, Value>;

/// Accounts used by these tests: `reader` (read), `writer` (write), `both`
/// and `both2` (read + write), `admin` (admin on varrow), all trust accounts
/// on `varrow`; `root` is the bootstrap admin.
fn accounts(dir: &std::path::Path) {
    for (name, grants) in [
        ("reader", vec!["read"]),
        ("writer", vec!["write"]),
        ("both", vec!["read", "write"]),
        ("both2", vec!["read", "write"]),
        ("admin", vec!["admin"]),
    ] {
        let account = format!("{name}@localhost");
        kala_system::system::add(dir, &account, None).unwrap();
        for grant in grants {
            kala_system::system::grant(dir, &account, grant, "varrow").unwrap();
        }
    }
}

fn server_with(mut config: ServerConfig) -> Running {
    let dir = tempfile::tempdir().unwrap();
    drop(kala_fixture::load_fixture_to(VARROW, &dir.path().join("varrow.kala")).unwrap());
    drop(kala_fixture::load_fixture_to(VARROW, &dir.path().join("other.kala")).unwrap());
    accounts(dir.path());
    config.listen = vec!["127.0.0.1:0".parse().unwrap()];
    config.default_db = Some("varrow".into());
    let server = Server::bind_accounts(dir.path(), config).unwrap();
    Running::from_server(dir, server)
}

fn server() -> Running {
    server_with(ServerConfig::default())
}

fn login(server: &Running, user: &str) -> Client {
    let mut client = Client::connected(server, 4);
    client.hello();
    client.success();
    client.send(
        0x6a,
        vec![map(&[
            ("scheme", string("basic")),
            ("principal", string(user)),
            ("credentials", string("")),
        ])],
    );
    client.success();
    client
}

fn world(server: &Running) -> World {
    World::open_read_only(&server.dir.path().join("varrow.kala")).unwrap()
}

fn begin(client: &mut Client) {
    client.send(0x11, vec![map(&[])]);
    client.success();
}

fn datalog(body: &str) -> String {
    format!("// kala:datalog\n{body}")
}

fn send_run(client: &mut Client, text: &str, extra: Value) {
    client.send(0x10, vec![string(text), map(&[]), extra]);
}

fn write(client: &mut Client, body: &str) -> (Vec<Vec<Value>>, Meta) {
    client.run_empty(&datalog(body));
    client.pull(-1)
}

fn commit(client: &mut Client) -> Meta {
    client.send(0x12, vec![]);
    client.success()
}

fn any_failure(client: &mut Client) -> Meta {
    client::metadata(client.recv(), 0x7f)
}

fn code(meta: &Meta) -> &str {
    let Some(Value::String(code)) = meta.get("code") else {
        panic!("missing failure code: {meta:?}")
    };
    code
}

fn text(meta: &Meta) -> &str {
    let Some(Value::String(text)) = meta.get("message") else {
        panic!("missing failure message: {meta:?}")
    };
    text
}

fn transient(meta: &Meta) -> bool {
    code(meta).starts_with("Neo.TransientError.")
}

// ---------------------------------------------------------------- A

#[test]
fn bookmarks_for_other_databases_are_ignored_and_malformed_ones_refused() {
    let server = server();
    let mut root = Client::ready(&server);
    // A driver's bookmark manager sends every bookmark it holds, whatever
    // the target database. `kala:varrow:7` is far beyond `other`'s history:
    // had it been applied to `other`, the request would wait and fail.
    let foreign = Value::List(vec![string("kala:varrow:999999")]);
    for db in ["other", "system"] {
        let query = if db == "system" {
            "SHOW DATABASES"
        } else {
            ALIASES
        };
        send_run(
            &mut root,
            query,
            map(&[("db", string(db)), ("bookmarks", foreign.clone())]),
        );
        let response = root.recv();
        assert!(
            matches!(response, Value::Structure(0x70, _)),
            "{db}: {response:?}"
        );
        root.pull(-1);
        root.send(
            0x11,
            vec![map(&[
                ("db", string(db)),
                ("mode", string("r")),
                ("bookmarks", foreign.clone()),
            ])],
        );
        let response = root.recv();
        assert!(
            matches!(response, Value::Structure(0x70, _)),
            "{db}: {response:?}"
        );
        commit(&mut root);
    }
    // Its own database's bookmark still counts, also next to a foreign one.
    let (_, summary) = write(&mut root, "[:assert :greg :alias \"Causal\" :at 300]");
    send_run(
        &mut root,
        ALIASES,
        map(&[(
            "bookmarks",
            Value::List(vec![
                string("kala:other:999999"),
                summary["bookmark"].clone(),
            ]),
        )]),
    );
    root.success();
    assert!(root.pull(-1).0.contains(&vec![string("Causal")]));
    for bad in [
        "invalid",
        "kala:varrow",
        "kala::7",
        "kala:varrow:",
        "kala:varrow:-1",
        "kala:varrow:7x",
        "other:varrow:7",
        "kala:other:x",
        "kala:not a name:7",
    ] {
        for db in ["varrow", "other"] {
            send_run(
                &mut root,
                ALIASES,
                map(&[
                    ("db", string(db)),
                    ("bookmarks", Value::List(vec![string(bad)])),
                ]),
            );
            let failure = any_failure(&mut root);
            assert_eq!(code(&failure), BOOKMARK, "{bad:?} on {db}");
            root.reset();
        }
    }
}

// ---------------------------------------------------------------- B

fn timeouts(total: Duration, idle: Duration) -> ServerConfig {
    ServerConfig {
        tx_timeout: total,
        tx_idle_timeout: idle,
        busy_timeout: Duration::from_secs(3),
        ..ServerConfig::default()
    }
}

/// Holds the world's writer lock (as another process would) while `during`
/// runs and for `time` after it.
fn hold_lock_for(server: &Running, time: Duration, during: impl FnOnce()) {
    let mut holder = World::open(&server.dir.path().join("varrow.kala")).unwrap();
    holder
        .with_writer_lock(|| {
            during();
            std::thread::sleep(time);
        })
        .unwrap();
}

#[test]
fn a_commit_slower_than_the_idle_timeout_is_not_idle() {
    let server = server_with(timeouts(Duration::from_secs(3), Duration::from_millis(300)));
    let mut client = Client::ready(&server);
    begin(&mut client);
    write(
        &mut client,
        "[:assert :greg :alias \"Slow commit\" :at 300]",
    );
    // COMMIT waits ~600 ms for the file lock: twice the idle timeout, well
    // inside the total one. The client is waiting, not idle.
    hold_lock_for(&server, Duration::from_millis(600), || {
        client.send(0x12, vec![]);
    });
    let response = client.recv();
    assert!(
        matches!(response, Value::Structure(0x70, _)),
        "{response:?}"
    );
    let greg = world(&server).entity_by_handle("greg").unwrap();
    assert!(
        world(&server)
            .as_of(greg, "alias", kala_core::Tick(300))
            .unwrap()
            .contains(&kala_core::Value::Str("Slow commit".into()))
    );
}

#[test]
fn a_response_slower_than_the_idle_timeout_is_not_idle() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("varrow.kala");
    let mut seeded = kala_fixture::load_fixture_to(VARROW, &path).unwrap();
    let greg = seeded.entity_by_handle("greg").unwrap();
    seeded
        .assert(
            greg,
            "biography",
            kala_core::Value::Str("x".repeat(300_000)),
            Some(kala_core::Tick(800)),
        )
        .unwrap();
    drop(seeded);
    let mut config = timeouts(Duration::from_secs(8), Duration::from_millis(300));
    config.listen = vec!["127.0.0.1:0".parse().unwrap()];
    config.default_db = Some("varrow".into());
    let bound = Server::bind_accounts(dir.path(), config).unwrap();
    let server = Running::from_server(dir, bound);
    let mut client = Client::ready(&server);
    begin(&mut client);
    client.run_empty(&datalog(
        "[:find ?n ?b :where [?e :name ?n] [:greg :biography ?b]]",
    ));
    client.send(0x3f, vec![map(&[("n", Value::Integer(-1))])]);
    // Megabytes of records wait for a client that reads nothing for twice
    // the idle timeout: the server is blocked sending, the client isn't idle
    // in the transaction's sense (the write timeout bounds a stuck reader).
    std::thread::sleep(Duration::from_millis(600));
    let (rows, _) = client.records();
    assert!(rows.len() > 5);
    commit(&mut client);
}

#[test]
fn an_idle_gap_longer_than_the_idle_timeout_still_expires_the_transaction() {
    let server = server_with(timeouts(Duration::from_secs(5), Duration::from_millis(300)));
    let mut client = Client::ready(&server);
    begin(&mut client);
    // Requests closer together than the idle timeout keep it alive, however
    // many there are: the clock restarts with each response.
    for _ in 0..4 {
        std::thread::sleep(Duration::from_millis(150));
        assert_eq!(client.query(client::ALLIES).len(), 2);
    }
    std::thread::sleep(Duration::from_millis(600));
    send_run(&mut client, client::ALLIES, map(&[]));
    client.failure(TIMED_OUT);
    client.reset();
    assert_eq!(client.query(client::ALLIES).len(), 2);
}

// ---------------------------------------------------------------- C

fn expired_transaction(server: &Running) -> Client {
    let mut client = Client::ready(server);
    begin(&mut client);
    write(&mut client, "[:create :CHARACTER :expired_here]");
    std::thread::sleep(Duration::from_millis(600));
    client
}

#[test]
fn reset_after_a_transaction_expired_succeeds() {
    let server = server_with(timeouts(Duration::from_secs(5), Duration::from_millis(200)));
    let mut client = expired_transaction(&server);
    // RESET always succeeds; the expiry is not an error of the RESET.
    client.reset();
    assert_eq!(client.query(client::ALLIES).len(), 2);
    assert!(world(&server).entity_by_handle("expired_here").is_none());
}

#[test]
fn goodbye_after_a_transaction_expired_closes_without_a_failure() {
    let server = server_with(timeouts(Duration::from_secs(5), Duration::from_millis(200)));
    let mut client = expired_transaction(&server);
    client.send(0x02, vec![]);
    common::assert_closed(&mut client.stream);
}

#[test]
fn any_other_request_after_a_transaction_expired_gets_the_timeout_failure() {
    let server = server_with(timeouts(Duration::from_secs(5), Duration::from_millis(200)));
    for (tag, fields) in [
        (0x12, vec![]),
        (0x13, vec![]),
        (0x10, vec![string(client::ALLIES), map(&[]), map(&[])]),
        (0x11, vec![map(&[])]),
    ] {
        let mut client = expired_transaction(&server);
        client.send(tag, fields);
        client.failure(TIMED_OUT);
        // Failed: requests are ignored until RESET.
        send_run(&mut client, client::ALLIES, map(&[]));
        client::structure(client.recv(), 0x7e);
        client.reset();
        assert_eq!(client.query(client::ALLIES).len(), 2);
    }
    assert!(world(&server).entity_by_handle("expired_here").is_none());
}
