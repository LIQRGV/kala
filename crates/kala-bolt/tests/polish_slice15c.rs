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
            Value::List(vec![string("kala:other:999999"), summary["bookmark"].clone()]),
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
