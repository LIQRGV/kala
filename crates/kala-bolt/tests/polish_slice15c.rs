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

fn server_with(config: ServerConfig) -> Running {
    counted_server(config).0
}

/// A server and the counters of how often it replayed a world's log.
fn counted_server(mut config: ServerConfig) -> (Running, kala_bolt::OpenCounts) {
    let dir = tempfile::tempdir().unwrap();
    drop(kala_fixture::load_fixture_to(VARROW, &dir.path().join("varrow.kala")).unwrap());
    drop(kala_fixture::load_fixture_to(VARROW, &dir.path().join("other.kala")).unwrap());
    accounts(dir.path());
    config.listen = vec!["127.0.0.1:0".parse().unwrap()];
    config.default_db = Some("varrow".into());
    let server = Server::bind_accounts(dir.path(), config).unwrap();
    let counts = server.open_counts();
    (Running::from_server(dir, server), counts)
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

// ---------------------------------------------------------------- D

#[test]
fn a_failed_write_keeps_the_cached_writer_and_a_diverged_log_reopens_it() {
    let (server, counts) = counted_server(ServerConfig::default());
    let path = server.dir.path().join("varrow.kala");
    let original = std::fs::read(&path).unwrap();
    let mut client = Client::ready(&server);
    write(&mut client, "[:assert :greg :alias \"First\" :at 300]");
    assert_eq!(counts.writer("varrow"), 1);
    // Ordinary client errors: none of them damages the handle, so none may
    // cost the next writer a replay of the whole log.
    for bad in [
        "[:assert :nobody_here :alias \"x\" :at 300]",
        "[:retract :greg :alias \"First\" :at 300]",
        "[:assert :greg :alias 7 :at 300]",
        "[:assert :greg :alias \"ok\" :at 301] [:assert :greg",
        "[:create :CHARACTER :greg]",
    ] {
        send_run(&mut client, &datalog(bad), map(&[]));
        let failure = any_failure(&mut client);
        assert!(
            code(&failure).starts_with("Neo.ClientError."),
            "{failure:?}"
        );
        client.reset();
    }
    let mut reader = login(&server, "reader");
    send_run(&mut reader, WRITE, map(&[]));
    reader.failure(FORBIDDEN);
    write(&mut client, "[:assert :greg :alias \"Second\" :at 300]");
    assert_eq!(
        counts.writer("varrow"),
        1,
        "a client error must not drop the cached writer"
    );
    // Another process replaced the log: this handle's history is gone, so
    // it is reopened and the retry works on the new file.
    std::fs::write(&path, &original).unwrap();
    send_run(&mut client, WRITE, map(&[]));
    let failure = any_failure(&mut client);
    assert!(transient(&failure), "{failure:?}");
    client.reset();
    write(&mut client, "[:assert :greg :alias \"Third\" :at 300]");
    assert_eq!(counts.writer("varrow"), 2);
    let greg = world(&server).entity_by_handle("greg").unwrap();
    let held = world(&server)
        .as_of(greg, "alias", kala_core::Tick(300))
        .unwrap();
    assert!(held.contains(&kala_core::Value::Str("Third".into())));
    assert!(!held.contains(&kala_core::Value::Str("Second".into())));
}

#[test]
fn sequential_auto_commit_writes_replay_the_log_once_not_once_per_write() {
    let (server, counts) = counted_server(ServerConfig {
        busy_timeout: Duration::from_millis(300),
        ..ServerConfig::default()
    });
    let mut client = Client::ready(&server);
    let mut bookmark = None;
    for n in 0..12 {
        // Each write names the previous one's bookmark, as a driver session
        // does: the check must not cost a replay either.
        let extra = bookmark
            .take()
            .map_or_else(|| map(&[]), |b| map(&[("bookmarks", Value::List(vec![b]))]));
        client.run(
            &datalog(&format!("[:assert :greg :alias \"Write {n}\" :at 300]")),
            map(&[]),
            extra,
        );
        let (_, summary) = client.pull(-1);
        bookmark = Some(summary["bookmark"].clone());
    }
    assert_eq!(counts.writer("varrow"), 1);
    assert_eq!(
        counts.reader("varrow"),
        0,
        "an auto-commit write must not reload the reader snapshot"
    );
    // A bookmark from the future is still refused after the bounded wait.
    let start = Instant::now();
    send_run(
        &mut client,
        WRITE,
        map(&[(
            "bookmarks",
            Value::List(vec![string("kala:varrow:999999999")]),
        )]),
    );
    let failure = any_failure(&mut client);
    assert!(transient(&failure), "{failure:?}");
    assert!(text(&failure).contains("bookmark"), "{failure:?}");
    assert!(start.elapsed() < Duration::from_secs(3));
    client.reset();
    // And a reader given the last write's bookmark sees it: one reload.
    send_run(
        &mut client,
        ALIASES,
        map(&[("bookmarks", Value::List(vec![bookmark.unwrap()]))]),
    );
    client.success();
    assert!(client.pull(-1).0.contains(&vec![string("Write 11")]));
    assert_eq!(counts.reader("varrow"), 1);
}

// ---------------------------------------------------------------- E

const TOO_MANY: &str = "too many open write transactions";

#[test]
fn default_mode_transactions_that_only_query_take_no_slot_and_open_no_handle() {
    let (server, counts) = counted_server(ServerConfig::default());
    // Drivers begin every transaction in write mode unless told otherwise.
    // Nine of them, all reading: one more than the write-transaction cap.
    let mut clients: Vec<_> = (0..9).map(|_| login(&server, "both")).collect();
    for client in &mut clients {
        client.send(0x11, vec![map(&[("mode", string("w"))])]);
        let response = client.recv();
        assert!(
            matches!(response, Value::Structure(0x70, _)),
            "{response:?}"
        );
        assert_eq!(client.query(client::ALLIES).len(), 2);
    }
    assert_eq!(
        counts.writer("varrow"),
        0,
        "a transaction that never writes must not replay a private handle"
    );
    for client in &mut clients {
        let summary = commit(client);
        assert_eq!(summary.get("type"), Some(&string("r")));
    }
}

#[test]
fn write_slots_are_taken_at_the_first_write_and_capped_per_account() {
    let server = server();
    let staged = |client: &mut Client, n: usize| {
        begin(client);
        send_run(
            client,
            &datalog(&format!("[:create :CHARACTER :slot_{n}]")),
            map(&[]),
        );
        client.recv()
    };
    // The cap is 8, so one account may hold 4.
    let mut first: Vec<_> = (0..5).map(|_| login(&server, "both")).collect();
    for (n, client) in first.iter_mut().enumerate().take(4) {
        let response = staged(client, n);
        assert!(
            matches!(response, Value::Structure(0x70, _)),
            "{response:?}"
        );
        client.pull(-1);
    }
    let refused = client::metadata(staged(&mut first[4], 4), 0x7f);
    assert!(transient(&refused), "{refused:?}");
    assert!(text(&refused).contains(TOO_MANY), "{refused:?}");
    assert!(text(&refused).contains("account"), "{refused:?}");
    first[4].reset();
    // Another account is not locked out by the first one.
    let mut second: Vec<_> = (0..4).map(|_| login(&server, "both2")).collect();
    for (n, client) in second.iter_mut().enumerate() {
        let response = staged(client, 10 + n);
        assert!(
            matches!(response, Value::Structure(0x70, _)),
            "{response:?}"
        );
        client.pull(-1);
    }
    // All 8 slots are held: the ninth writing transaction is refused,
    // whoever it is, while a ninth reading one is not.
    let mut root = Client::ready(&server);
    let refused = client::metadata(staged(&mut root, 20), 0x7f);
    assert!(transient(&refused), "{refused:?}");
    assert!(text(&refused).contains(TOO_MANY), "{refused:?}");
    root.reset();
    begin(&mut root);
    assert_eq!(root.query(client::ALLIES).len(), 2);
    commit(&mut root);
    // A slot is free again as soon as its transaction ends.
    commit(&mut second[0]);
    let response = staged(&mut root, 21);
    assert!(
        matches!(response, Value::Structure(0x70, _)),
        "{response:?}"
    );
    root.pull(-1);
    commit(&mut root);
    assert!(world(&server).entity_by_handle("slot_10").is_some());
    assert!(world(&server).entity_by_handle("slot_21").is_some());
    assert!(world(&server).entity_by_handle("slot_0").is_none());
}

#[test]
fn a_transaction_that_queried_before_another_commit_conflicts_when_it_then_writes() {
    let server = server();
    let mut a = login(&server, "both");
    let mut b = login(&server, "both2");
    let names = "// kala:datalog\n[:find ?n :at -210 :where [:edwin :name ?n]]";
    for weak in [false, true] {
        a.send(
            0x11,
            vec![if weak {
                map(&[("tx_metadata", map(&[("concurrency_mode", string("weak"))]))])
            } else {
                map(&[])
            }],
        );
        a.success();
        assert!(!a.query(names).is_empty());
        // Another connection commits something the query did not read. The
        // writable handle is opened at A's first write, on the newer world:
        // what A's queries saw is no longer what its writes are checked
        // against, so the commit is refused (and retried by drivers).
        write(
            &mut b,
            &format!("[:assert :mara :alias \"Elsewhere {weak}\" :at 300]"),
        );
        write(
            &mut a,
            &format!("[:assert :greg :alias \"After reading {weak}\" :at 300]"),
        );
        a.send(0x12, vec![]);
        if weak {
            a.success();
            continue;
        }
        let failure = any_failure(&mut a);
        assert!(transient(&failure), "{failure:?}");
        assert!(
            text(&failure).contains("nothing was written"),
            "{failure:?}"
        );
        a.reset();
        let greg = world(&server).entity_by_handle("greg").unwrap();
        let held = |server: &Running| {
            world(server)
                .as_of(greg, "alias", kala_core::Tick(300))
                .unwrap()
        };
        assert!(!held(&server).contains(&kala_core::Value::Str("After reading false".into())));
        // The retry reads the newer world and commits.
        begin(&mut a);
        assert!(!a.query(names).is_empty());
        write(
            &mut a,
            "[:assert :greg :alias \"After reading false\" :at 300]",
        );
        commit(&mut a);
        assert!(held(&server).contains(&kala_core::Value::Str("After reading false".into())));
    }
    // A transaction that has not queried is not affected: its snapshot is
    // simply the world at its first write.
    begin(&mut a);
    write(&mut b, "[:assert :mara :alias \"Elsewhere again\" :at 300]");
    write(&mut a, "[:assert :greg :alias \"Blind\" :at 300]");
    commit(&mut a);
}

#[test]
fn default_mode_begin_is_refused_only_when_a_write_form_arrives() {
    // An admin on `system`: drivers' execute_query("SHOW DATABASES",
    // database_="system") begins a default (write) mode transaction.
    let server = server();
    let system = std::fs::read(server.dir.path().join("system.kala")).unwrap();
    let mut root = Client::ready(&server);
    root.send(0x11, vec![map(&[("db", string("system"))])]);
    let response = root.recv();
    assert!(
        matches!(response, Value::Structure(0x70, _)),
        "{response:?}"
    );
    assert!(root.query("SHOW DATABASES").len() >= 3);
    commit(&mut root);
    root.send(0x11, vec![map(&[("db", string("system"))])]);
    root.success();
    send_run(
        &mut root,
        &datalog("[:create :ACCOUNT :backdoor]"),
        map(&[]),
    );
    let failure = root.failure(FORBIDDEN);
    assert!(text(&failure).contains("kala user"), "{failure:?}");
    root.reset();
    assert_eq!(
        std::fs::read(server.dir.path().join("system.kala")).unwrap(),
        system
    );
    // A read-only identity.
    let mut reader = login(&server, "reader");
    begin(&mut reader);
    assert_eq!(reader.query(client::ALLIES).len(), 2);
    send_run(&mut reader, WRITE, map(&[]));
    let failure = reader.failure(FORBIDDEN);
    assert!(text(&failure).contains("write privilege"), "{failure:?}");
    reader.reset();
    // A read-only server.
    let server = server_with(ServerConfig {
        read_only: true,
        ..ServerConfig::default()
    });
    let before = std::fs::read(server.dir.path().join("varrow.kala")).unwrap();
    let mut client = Client::ready(&server);
    client.send(0x11, vec![map(&[("mode", string("w"))])]);
    let response = client.recv();
    assert!(
        matches!(response, Value::Structure(0x70, _)),
        "{response:?}"
    );
    assert_eq!(client.query(client::ALLIES).len(), 2);
    send_run(&mut client, WRITE, map(&[]));
    let failure = client.failure(FORBIDDEN);
    assert!(
        text(&failure).contains("this server is read-only"),
        "{failure:?}"
    );
    client.reset();
    assert_eq!(
        std::fs::read(server.dir.path().join("varrow.kala")).unwrap(),
        before
    );
}

// ---------------------------------------------------------------- F

/// The five forms that change what existing data or the world's rules mean,
/// each on its own attribute so the admin can apply them all.
const ADMIN_FORMS: [&str; 5] = [
    "[:world-setting :tick_unit :day]",
    "[:rename :f_renamed :f_renamed_to]",
    "[:deprecate :f_deprecated]",
    "[:cardinality :f_cardinality :many]",
    "[:migrate :f_migrated :i64]",
];

#[test]
fn schema_changes_and_settings_need_admin_and_declarations_need_only_write() {
    let server = server();
    let path = server.dir.path().join("varrow.kala");
    // Declarations (explicit and inferred) need only `write`.
    let mut writer = login(&server, "writer");
    for declaration in [
        "[:attribute :f_renamed :string]",
        "[:attribute :f_deprecated :string]",
        "[:attribute :f_cardinality :string]",
        "[:attribute :f_migrated :string]",
        "[:node-type :FACTION]",
        "[:assert :greg :f_inferred 1 :at 300]",
    ] {
        let (_, summary) = write(&mut writer, declaration);
        assert!(summary.contains_key("bookmark"), "{declaration}");
    }
    let declared = world(&server);
    assert!(declared.node_types().any(|t| t == "FACTION"));
    drop(declared);
    // Without admin: refused before anything is staged, naming `admin`, in
    // auto-commit, in an explicit transaction and hidden behind another op.
    for user in ["writer", "both"] {
        let mut client = login(&server, user);
        for form in ADMIN_FORMS {
            for shape in 0..3 {
                let before = std::fs::read(&path).unwrap();
                let body = if shape == 2 {
                    format!("[[:assert :greg :alias \"Hidden\" :at 300] {form}]")
                } else {
                    form.to_owned()
                };
                if shape == 1 {
                    begin(&mut client);
                }
                send_run(&mut client, &datalog(&body), map(&[]));
                let failure = client.failure(FORBIDDEN);
                assert!(
                    text(&failure).contains("admin"),
                    "{user} {form}: {failure:?}"
                );
                client.reset();
                assert_eq!(std::fs::read(&path).unwrap(), before, "{user} {form}");
            }
        }
    }
    // With admin on the database (or on every database): all five apply.
    let mut admin = login(&server, "admin");
    for (n, form) in ADMIN_FORMS.into_iter().enumerate() {
        if n % 2 == 0 {
            write(&mut admin, form);
        } else {
            begin(&mut admin);
            write(&mut admin, form);
            commit(&mut admin);
        }
    }
    let changed = world(&server);
    assert_eq!(changed.world_setting("tick_unit"), Some("day"));
    drop(changed);
    let mut root = Client::ready(&server);
    write(&mut root, "[:world-setting :schema :strict]");
    write(&mut root, "[:world-setting :schema :infer]");
    // `admin` on varrow is not admin elsewhere.
    send_run(
        &mut admin,
        &datalog("[:world-setting :schema :strict]"),
        map(&[("db", string("other"))]),
    );
    admin.failure(FORBIDDEN);
}

#[test]
fn an_unknown_world_setting_key_is_refused_with_the_known_keys() {
    let server = server();
    let path = server.dir.path().join("varrow.kala");
    let before = std::fs::read(&path).unwrap();
    let mut root = Client::ready(&server);
    for explicit in [false, true] {
        if explicit {
            begin(&mut root);
        }
        send_run(
            &mut root,
            &datalog("[:world-setting :anything_at_all :x]"),
            map(&[]),
        );
        let failure = any_failure(&mut root);
        assert!(
            code(&failure).starts_with("Neo.ClientError.Statement."),
            "{failure:?}"
        );
        for known in ["schema", "concurrency", "tick_unit"] {
            assert!(text(&failure).contains(known), "{failure:?}");
        }
        root.reset();
        assert_eq!(std::fs::read(&path).unwrap(), before);
    }
}

// ---------------------------------------------------------------- G

#[test]
fn a_write_only_identity_gets_one_code_and_message_for_every_store_rule_failure() {
    let server = server();
    let mut root = Client::ready(&server);
    write(&mut root, "[:assert :greg :alias \"Held\" :at 300]");
    // Each fails for a different reason that depends on what the world
    // holds: an existing handle, a held value, a declared type, a missing
    // handle, a handle of another type.
    let failing = [
        "[:create :CHARACTER :greg]",
        "[:retract :greg :alias \"Held\" :at 300]",
        "[:assert :greg :alias 7 :at 300]",
        "[:assert :nobody_here :alias \"x\" :at 300]",
        "[:assert :greg :located_in :nowhere_at_all :at 300]",
    ];
    let outcomes = |client: &mut Client| {
        let mut seen = Vec::new();
        for explicit in [false, true] {
            for form in failing {
                if explicit {
                    begin(client);
                }
                send_run(client, &datalog(form), map(&[]));
                let failure = any_failure(client);
                seen.push((code(&failure).to_owned(), text(&failure).to_owned()));
                client.reset();
            }
        }
        seen
    };
    // With `read`, the codes and messages tell the failures apart.
    let mut both = login(&server, "both");
    let detailed = outcomes(&mut both);
    let codes: std::collections::BTreeSet<_> = detailed.iter().map(|(c, _)| c.clone()).collect();
    assert!(codes.len() >= 3, "{detailed:?}");
    // Without it, they are indistinguishable: no membership oracle.
    let mut writer = login(&server, "writer");
    let reduced = outcomes(&mut writer);
    let distinct: std::collections::BTreeSet<_> = reduced.iter().cloned().collect();
    assert_eq!(distinct.len(), 1, "{reduced:?}");
    let (code, message) = &reduced[0];
    assert!(code.starts_with("Neo.ClientError.Statement."), "{code}");
    assert!(message.contains("read privilege"), "{message}");
    // Errors that describe the request, not the data, stay distinct.
    send_run(&mut writer, &datalog("[:assert :greg"), map(&[]));
    writer.failure("Neo.ClientError.Statement.SyntaxError");
    writer.reset();
    send_run(
        &mut writer,
        &datalog("[:set :greg :alias \"x\" :at 300]"),
        map(&[]),
    );
    writer.failure(FORBIDDEN);
    writer.reset();
    write(
        &mut writer,
        "[:assert :greg :alias \"Still writable\" :at 300]",
    );
}

// ---------------------------------------------------------------- I

#[test]
fn a_revoked_write_grant_or_dropped_account_stops_writes_on_open_connections() {
    let server = server();
    let dir = server.dir.path();
    let path = dir.join("varrow.kala");
    // Pooled driver connections stay logged in for a long time.
    let mut auto = login(&server, "both");
    let mut explicit = login(&server, "both");
    let mut dropped = login(&server, "both2");
    let mut promoted = login(&server, "reader");
    write(
        &mut auto,
        "[:assert :greg :alias \"Before revoke\" :at 300]",
    );
    kala_system::system::revoke(dir, "both@localhost", "write", "varrow").unwrap();
    kala_system::system::drop_account(dir, "both2@localhost").unwrap();
    kala_system::system::grant(dir, "reader@localhost", "write", "varrow").unwrap();
    let before = std::fs::read(&path).unwrap();
    // The next auto-commit write sees the revocation.
    send_run(&mut auto, WRITE, map(&[]));
    let failure = auto.failure(FORBIDDEN);
    assert!(text(&failure).contains("write privilege"), "{failure:?}");
    auto.reset();
    // So does a transaction begun after it.
    begin(&mut explicit);
    send_run(&mut explicit, WRITE, map(&[]));
    explicit.failure(FORBIDDEN);
    explicit.reset();
    // `read` was not revoked.
    assert_eq!(auto.query(client::ALLIES).len(), 2);
    // An account that no longer exists can neither begin nor write.
    dropped.send(0x11, vec![map(&[])]);
    dropped.failure(FORBIDDEN);
    dropped.reset();
    send_run(&mut dropped, WRITE, map(&[]));
    dropped.failure(FORBIDDEN);
    dropped.reset();
    assert_eq!(std::fs::read(&path).unwrap(), before);
    // A grant added meanwhile is picked up the same way.
    write(
        &mut promoted,
        "[:assert :greg :alias \"Granted later\" :at 300]",
    );
}

// ---------------------------------------------------------------- J

#[test]
fn a_query_after_schema_edits_in_its_transaction_still_conflicts_with_writes_it_read() {
    let server = server();
    let mut a = Client::ready(&server);
    let mut b = Client::ready(&server);
    for skewed in [true, false] {
        begin(&mut a);
        // In A's provisional schema `:alias` now names a brand-new
        // attribute; the committed `:alias` is `:alias_was`.
        write(
            &mut a,
            "[[:rename :alias :alias_was] [:attribute :alias :string]]",
        );
        // The query reads the COMMITTED world, where `:alias` is still the
        // old attribute: that is what A depends on.
        a.query(ALIASES);
        if skewed {
            write(&mut b, "[:assert :greg :alias \"Written under A\" :at 300]");
        }
        write(&mut a, "[:assert :mara :j_depends_on_aliases true :at 300]");
        a.send(0x12, vec![]);
        if skewed {
            let failure = any_failure(&mut a);
            assert!(transient(&failure), "{failure:?}");
            a.reset();
            let w = world(&server);
            assert!(w.entity_by_handle("mara").is_some());
            assert!(
                w.attributes().all(|def| def.name != "alias_was"),
                "the conflicting transaction must have written nothing"
            );
        } else {
            // Without the concurrent write the same transaction commits.
            a.success();
        }
    }
}
