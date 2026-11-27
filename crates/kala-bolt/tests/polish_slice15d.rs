//! PR #22 review follow-ups for Cypher writes over Bolt.
#[path = "common/client.rs"]
mod client;
mod common;

use client::{Client, map, string};
use common::{Running, VARROW};
use kala_bolt::packstream::Value;
use kala_bolt::{Server, ServerConfig};
use kala_core::{Tick, World};

const TYPE: &str = "Neo.ClientError.Statement.TypeError";

/// A server and the counters of how often it replayed a world's log.
fn counted_with(configure: impl FnOnce(&mut ServerConfig)) -> (Running, kala_bolt::OpenCounts) {
    let dir = tempfile::tempdir().unwrap();
    drop(kala_fixture::load_fixture_to(VARROW, &dir.path().join("varrow.kala")).unwrap());
    for (user, privilege) in [("reader", "read"), ("writer", "write")] {
        let account = format!("{user}@localhost");
        kala_system::system::add(dir.path(), &account, None).unwrap();
        kala_system::system::grant(dir.path(), &account, privilege, "varrow").unwrap();
    }
    let mut config = ServerConfig {
        listen: vec!["127.0.0.1:0".parse().unwrap()],
        default_db: Some("varrow".into()),
        ..ServerConfig::default()
    };
    configure(&mut config);
    let server = Server::bind_accounts(dir.path(), config).unwrap();
    let counts = server.open_counts();
    (Running::from_server(dir, server), counts)
}

fn server_with(configure: impl FnOnce(&mut ServerConfig)) -> Running {
    counted_with(configure).0
}

fn server() -> Running {
    server_with(|_| {})
}

fn world(server: &Running) -> World {
    World::open_read_only(&server.dir.path().join("varrow.kala")).unwrap()
}

fn log_bytes(server: &Running) -> Vec<u8> {
    std::fs::read(server.dir.path().join("varrow.kala")).unwrap()
}

fn begin(client: &mut Client) {
    client.send(0x11, vec![map(&[("db", string("varrow"))])]);
    client.success();
}

fn commit(client: &mut Client) {
    client.send(0x12, vec![]);
    client.success();
}

fn login(server: &Running, name: &str) -> Client {
    let mut client = Client::connected(server, 4);
    client.hello();
    client.success();
    client.send(
        0x6a,
        vec![map(&[
            ("scheme", string("basic")),
            ("principal", string(name)),
            ("credentials", string("")),
        ])],
    );
    client.success();
    client
}

fn failure(client: &mut Client, query: &str, params: Value) -> (String, String) {
    client.send(0x10, vec![string(query), params, map(&[])]);
    let m = client::metadata(client.recv(), 0x7f);
    let (Value::String(code), Value::String(message)) = (&m["code"], &m["message"]) else {
        panic!("{m:?}")
    };
    client.reset();
    (code.clone(), message.clone())
}

fn write(
    client: &mut Client,
    query: &str,
    params: Value,
) -> std::collections::BTreeMap<String, Value> {
    client.run(query, params, map(&[]));
    client.pull(-1).1
}

fn changed(summary: &std::collections::BTreeMap<String, Value>) -> i64 {
    let Value::Map(kala) = &summary["kala"] else {
        panic!("{summary:?}")
    };
    let Value::Integer(n) = kala["changed"] else {
        panic!("{kala:?}")
    };
    n
}

// ---- A: parameters are bound with the kinds the reading part computed ----

#[test]
fn a_handle_parameter_in_a_ref_position_of_the_reading_part_finds_its_rows() {
    for explicit in [false, true] {
        let server = server();
        let mut client = Client::ready(&server);
        // The same pattern as a read query finds Greg (in Ashford from 601).
        client.run(
            "MATCH (c:CHARACTER {located_in:$place}) AT TICK 700 RETURN c.kala_handle",
            map(&[("place", string("ashford"))]),
            map(&[]),
        );
        let found = client.pull(-1).0;
        assert!(found.contains(&vec![string("greg")]), "{found:?}");
        if explicit {
            begin(&mut client);
        }
        let summary = write(
            &mut client,
            "MATCH (c:CHARACTER {located_in:$place}) SET c.hp = 0 AT TICK 700",
            map(&[("place", string("ashford"))]),
        );
        if explicit {
            commit(&mut client);
        } else {
            assert_eq!(changed(&summary), found.len() as i64, "{summary:?}");
        }
        let committed = world(&server);
        let greg = committed.entity_by_handle("greg").unwrap();
        assert_eq!(
            committed.as_of(greg, "hp", Tick(700)).unwrap(),
            [kala_core::Value::I64(0)],
            "explicit={explicit}"
        );
        let mara = committed.entity_by_handle("mara").unwrap();
        if !found.contains(&vec![string("mara")]) {
            assert!(committed.as_of(mara, "hp", Tick(700)).unwrap().is_empty());
        }
    }
}

#[test]
fn a_wrongly_typed_parameter_is_a_type_error_before_evaluation() {
    let server = server();
    let before = log_bytes(&server);
    let mut client = Client::ready(&server);
    for explicit in [false, true] {
        if explicit {
            begin(&mut client);
        }
        for (query, params) in [
            (
                "MATCH (c:CHARACTER {located_in:$place}) SET c.hp = 0 AT TICK 700",
                map(&[("place", Value::Integer(5))]),
            ),
            (
                "MATCH (c:CHARACTER {located_in:$place}) SET c.hp = 0 AT TICK 700",
                map(&[("place", string("no-such-place"))]),
            ),
            (
                "MATCH (c {kala_handle:'greg'}) SET c.hp = 1 AT TICK $t",
                map(&[("t", string("soon"))]),
            ),
        ] {
            let (code, message) = failure(&mut client, query, params);
            assert_eq!(code, TYPE, "{query}: {message}");
            if explicit {
                begin(&mut client);
            }
        }
        if explicit {
            client.send(0x13, vec![]);
            client.success();
        }
    }
    assert!(log_bytes(&server) == before);
}

#[test]
fn a_handle_string_written_to_a_ref_attribute_resolves_to_the_entity() {
    let server = server();
    let mut client = Client::ready(&server);
    let summary = write(
        &mut client,
        "MATCH (c {kala_handle:'greg'}) SET c.located_in = $place AT TICK 700",
        map(&[("place", string("varrow"))]),
    );
    assert_eq!(changed(&summary), 2, "{summary:?}");
    let committed = world(&server);
    let greg = committed.entity_by_handle("greg").unwrap();
    let varrow = committed.entity_by_handle("varrow").unwrap();
    assert_eq!(
        committed.as_of(greg, "located_in", Tick(700)).unwrap(),
        [kala_core::Value::Ref(varrow)]
    );
    drop(committed);
    let before = log_bytes(&server);
    let (code, message) = failure(
        &mut client,
        "MATCH (c {kala_handle:'greg'}) SET c.located_in = $place AT TICK 701",
        map(&[("place", string("nowhere"))]),
    );
    assert!(
        message.contains("unknown entity `nowhere`"),
        "{code}: {message}"
    );
    assert!(!code.starts_with("Neo.TransientError"), "{code}");
    assert!(log_bytes(&server) == before);

    // A handle created earlier in the same transaction resolves too, in a
    // SET and in a CREATE property map.
    begin(&mut client);
    client.query("CREATE (:PLACE {kala_handle:'newtown'})");
    client.run(
        "MATCH (c {kala_handle:'mara'}) SET c.located_in = $place AT TICK 702",
        map(&[("place", string("newtown"))]),
        map(&[]),
    );
    client.pull(-1);
    client.run(
        "CREATE (:CHARACTER {kala_handle:'settler', located_in:$place}) AT TICK 702",
        map(&[("place", string("newtown"))]),
        map(&[]),
    );
    client.pull(-1);
    commit(&mut client);
    let committed = world(&server);
    let newtown = committed.entity_by_handle("newtown").unwrap();
    for who in ["mara", "settler"] {
        let id = committed.entity_by_handle(who).unwrap();
        assert_eq!(
            committed.as_of(id, "located_in", Tick(702)).unwrap(),
            [kala_core::Value::Ref(newtown)],
            "{who}"
        );
    }
}

// ---- B: the reading part of an auto-commit write runs outside the writer mutex ----

const DECLARE_HP: &str = "// kala:datalog\n[:attribute :hp :i64]";

/// Loads the server's reader snapshot with a read query, as any session
/// that has queried does. An auto-commit write reads off the writer mutex
/// only on a reader snapshot that is already there and current (it never
/// replays the log for one), and the hook below is called only then.
fn warm(client: &mut Client) {
    assert_eq!(
        client
            .query("MATCH (g {kala_handle:'greg'}) RETURN g.name")
            .len(),
        1
    );
}

/// A hook that blocks the first statement reaching it until released.
fn gate() -> (
    kala_bolt::Hook,
    std::sync::mpsc::Receiver<()>,
    std::sync::mpsc::Sender<()>,
) {
    let (reached_tx, reached_rx) = std::sync::mpsc::channel();
    let (go_tx, go_rx) = std::sync::mpsc::channel::<()>();
    let go_rx = std::sync::Mutex::new(Some(go_rx));
    let reached_tx = std::sync::Mutex::new(reached_tx);
    let hook = kala_bolt::Hook(std::sync::Arc::new(move || {
        // Only the first statement waits; later ones pass straight through.
        let first = go_rx.lock().unwrap().take();
        if let Some(go) = first {
            reached_tx.lock().unwrap().send(()).unwrap();
            let _ = go.recv_timeout(std::time::Duration::from_secs(10));
        }
    }));
    (hook, reached_rx, go_tx)
}

#[test]
fn another_writer_commits_while_an_auto_commit_reading_part_is_in_flight() {
    let (hook, reached, go) = gate();
    let server = server_with(|config| {
        config.after_statement_read = Some(hook);
        config.busy_timeout = std::time::Duration::from_millis(400);
    });
    let mut first = Client::ready(&server);
    let mut second = Client::ready(&server);
    warm(&mut second);
    first.send(
        0x10,
        vec![
            string("MATCH (c:CHARACTER) SET c.checked = true AT TICK 900"),
            map(&[]),
            map(&[]),
        ],
    );
    reached
        .recv_timeout(std::time::Duration::from_secs(5))
        .unwrap();
    // The first statement has read and not yet staged: nobody holds the
    // database's writer, so this is not a lock timeout.
    let summary = write(
        &mut second,
        "CREATE (:PLACE {kala_handle:'meanwhile'})",
        map(&[]),
    );
    assert_eq!(summary["type"], string("w"));
    assert!(world(&server).entity_by_handle("meanwhile").is_some());
    go.send(()).unwrap();
    first.success();
    let summary = first.pull(-1).1;
    assert_eq!(changed(&summary), 33, "{summary:?}");
}

#[test]
fn a_commit_between_the_read_and_the_stage_conflicts_only_on_what_was_read() {
    for (other, conflicts) in [
        // Reads of g.hp are recorded per attribute: another attribute is free.
        (
            "MATCH (g {kala_handle:'greg'}) SET g.name = 'Gregor' AT TICK 900",
            false,
        ),
        (
            "MATCH (g {kala_handle:'greg'}) SET g.hp = 1 AT TICK 900",
            true,
        ),
        // A phantom: the reading part scanned the type.
        ("CREATE (:CHARACTER {kala_handle:'newcomer'})", true),
    ] {
        let (hook, reached, go) = gate();
        let server = server_with(|config| config.after_statement_read = Some(hook));
        let mut first = Client::ready(&server);
        let mut second = Client::ready(&server);
        // `hp` is declared up front: reading an undeclared name and then
        // seeing it declared is a conflict of its own.
        write(&mut second, DECLARE_HP, map(&[]));
        warm(&mut second);
        let statement = if other.starts_with("CREATE") {
            "MATCH (c:CHARACTER) SET c.hp = 7 AT TICK 901"
        } else {
            "MATCH (g {kala_handle:'greg'}) SET g.hp = g.hp AT TICK 901"
        };
        first.send(0x10, vec![string(statement), map(&[]), map(&[])]);
        reached
            .recv_timeout(std::time::Duration::from_secs(5))
            .unwrap();
        write(&mut second, other, map(&[]));
        let before = log_bytes(&server);
        go.send(()).unwrap();
        if conflicts {
            first.failure("Neo.TransientError.Transaction.DeadlockDetected");
            assert!(log_bytes(&server) == before, "{other}");
        } else {
            first.success();
            first.pull(-1);
        }
    }
}

#[test]
fn a_tick_less_write_lands_on_the_latest_tick_its_reading_part_saw() {
    let (hook, reached, go) = gate();
    let server = server_with(|config| config.after_statement_read = Some(hook));
    let mut first = Client::ready(&server);
    let mut second = Client::ready(&server);
    write(&mut second, DECLARE_HP, map(&[]));
    warm(&mut second);
    let read_tick = world(&server).latest_tick();
    first.send(
        0x10,
        vec![
            string("MATCH (g {kala_handle:'greg'}) SET g.hp = 3"),
            map(&[]),
            map(&[]),
        ],
    );
    reached
        .recv_timeout(std::time::Duration::from_secs(5))
        .unwrap();
    // Another commit moves the latest tick without touching what was read.
    write(
        &mut second,
        "MATCH (m {kala_handle:'mara'}) SET m.hp = 1 AT TICK 5000",
        map(&[]),
    );
    go.send(()).unwrap();
    first.success();
    let summary = first.pull(-1).1;
    let committed = world(&server);
    let greg = committed.entity_by_handle("greg").unwrap();
    assert_eq!(
        committed.as_of(greg, "hp", read_tick).unwrap(),
        [kala_core::Value::I64(3)]
    );
    let Value::Map(kala) = &summary["kala"] else {
        panic!("{summary:?}")
    };
    assert!(kala.contains_key("latest_tick_moved"), "{kala:?}");
}

// ---- F: RETURN is checked before the auto-commit write is durable ----

#[test]
fn an_auto_commit_return_that_cannot_be_rendered_commits_nothing() {
    let server = server_with(|config| config.max_message_bytes = 2048);
    let before = log_bytes(&server);
    let mut client = Client::ready(&server);
    let long = "x".repeat(700);
    let (code, message) = failure(
        &mut client,
        "CREATE (n:CHARACTER {kala_handle:'wordy', biography:$text}) RETURN n AS a, n AS b, n AS c, n AS d",
        map(&[("text", string(&long))]),
    );
    assert!(!message.contains("committed"), "{code}: {message}");
    assert!(!code.starts_with("Neo.TransientError"), "{code}");
    assert!(log_bytes(&server) == before, "{code}: {message}");
    assert!(world(&server).entity_by_handle("wordy").is_none());

    // The normal case: final IDs and the committed properties.
    client.run(
        "CREATE (n:CHARACTER {kala_handle:'terse', biography:$text}) RETURN n, n.biography",
        map(&[("text", string("short"))]),
        map(&[]),
    );
    let (rows, _) = client.pull(-1);
    let committed = world(&server);
    let terse = committed.entity_by_handle("terse").unwrap();
    let Value::Structure(0x4e, node) = &rows[0][0] else {
        panic!("{rows:?}")
    };
    assert_eq!(node[0], Value::Integer(terse.raw() as i64));
    assert_eq!(node[3], string(&committed.format_id(terse)));
    assert_eq!(rows[0][1], string("short"));
}

// ---- J: CREATE CONSTRAINT … IS :: TYPE over a socket ----

#[test]
fn a_type_constraint_needs_write_and_redefining_needs_admin() {
    let server = server();
    let declare = "CREATE CONSTRAINT FOR (n:CHARACTER) REQUIRE n.rank IS :: INTEGER";
    let mut reader = login(&server, "reader");
    let (code, message) = failure(&mut reader, declare, map(&[]));
    assert_eq!(code, "Neo.ClientError.Security.Forbidden", "{message}");
    assert!(message.contains("write"), "{message}");

    // A new attribute needs only `write` (no read): a schema statement.
    let mut writer = login(&server, "writer");
    let summary = write(&mut writer, declare, map(&[]));
    assert_eq!(summary["type"], string("s"), "{summary:?}");
    let Value::Map(kala) = &summary["kala"] else {
        panic!("{summary:?}")
    };
    assert_eq!(kala["declared"], Value::Integer(1));
    assert_eq!(
        world(&server).attribute("rank").unwrap().value_type,
        kala_core::ValueType::I64
    );
    // The same declaration again is a no-op for the same identity.
    let before = log_bytes(&server);
    let summary = write(
        &mut writer,
        "CREATE CONSTRAINT again IF NOT EXISTS FOR (n:CHARACTER) REQUIRE n.rank IS :: INT",
        map(&[]),
    );
    assert_eq!(summary["type"], string("s"));
    assert!(log_bytes(&server) == before);

    // Changing what the committed schema has is the administrator's.
    let redefine = "CREATE CONSTRAINT FOR (n:CHARACTER) REQUIRE n.rank IS :: STRING";
    for explicit in [false, true] {
        if explicit {
            begin(&mut writer);
        }
        let (code, message) = failure(&mut writer, redefine, map(&[]));
        assert_eq!(code, "Neo.ClientError.Security.Forbidden", "{message}");
        assert!(message.contains("admin"), "{message}");
    }
    assert!(log_bytes(&server) == before);
    let mut root = Client::ready(&server);
    begin(&mut root);
    root.run(redefine, map(&[]), map(&[]));
    let (rows, summary) = root.pull(-1);
    assert!(rows.is_empty());
    assert_eq!(summary["type"], string("s"));
    commit(&mut root);
    assert_eq!(
        world(&server).attribute("rank").unwrap().value_type,
        kala_core::ValueType::Str
    );
}

// ---- Second review, 1: an auto-commit Cypher write never replays the log ----

/// A hook that counts the statements whose reading part ran off the
/// writer mutex (the only place the hook is called).
fn counter() -> (
    kala_bolt::Hook,
    std::sync::Arc<std::sync::atomic::AtomicUsize>,
) {
    let count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let seen = std::sync::Arc::clone(&count);
    let hook = kala_bolt::Hook(std::sync::Arc::new(move || {
        seen.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }));
    (hook, count)
}

/// Runs `statements` one after another in one session, each naming the
/// previous one's bookmark as a driver session does.
fn chained(client: &mut Client, statements: impl Iterator<Item = String>) -> Value {
    let mut bookmark = None;
    for statement in statements {
        let extra = bookmark
            .take()
            .map_or_else(|| map(&[]), |b| map(&[("bookmarks", Value::List(vec![b]))]));
        client.run(&statement, map(&[]), extra);
        let (_, summary) = client.pull(-1);
        bookmark = Some(summary["bookmark"].clone());
    }
    bookmark.unwrap()
}

#[test]
fn sequential_blind_cypher_creates_replay_the_log_once_and_never_reload_the_reader() {
    let (server, counts) = counted_with(|_| {});
    let mut client = Client::ready(&server);
    chained(
        &mut client,
        (0..12).map(|n| format!("CREATE (:CHARACTER {{n: {n}}}) AT TICK 300")),
    );
    assert_eq!(counts.writer("varrow"), 1);
    assert_eq!(
        counts.reader("varrow"),
        0,
        "a blind auto-commit Cypher write must not reload the reader snapshot"
    );
}

#[test]
fn sequential_match_set_writes_replay_the_log_once_and_never_reload_the_reader() {
    let (hook, off_mutex) = counter();
    let (server, counts) = counted_with(|config| config.after_statement_read = Some(hook));
    let mut client = Client::ready(&server);
    write(&mut client, DECLARE_HP, map(&[]));
    // No reader snapshot is cached: each reading part runs on the writer.
    let set = |n: i64| {
        format!(
            "MATCH (g {{kala_handle:'greg'}}) SET g.hp = {n} AT TICK {}",
            900 + n
        )
    };
    chained(&mut client, (0..12).map(set));
    assert_eq!(counts.writer("varrow"), 1);
    assert_eq!(
        counts.reader("varrow"),
        0,
        "an auto-commit Cypher write must not reload the reader snapshot"
    );
    assert_eq!(off_mutex.load(std::sync::atomic::Ordering::SeqCst), 0);

    // A read query loads the reader snapshot (the one replay a reader
    // costs). From then on the write path keeps it current by applying the
    // log's tail to it, never by replaying the log: every reading part runs
    // off the writer mutex and the reader is not reloaded.
    warm(&mut client);
    assert_eq!(counts.reader("varrow"), 1);
    let bookmark = chained(&mut client, (12..24).map(set));
    assert_eq!(counts.writer("varrow"), 1);
    assert_eq!(counts.reader("varrow"), 1);
    assert_eq!(off_mutex.load(std::sync::atomic::Ordering::SeqCst), 12);
    let committed = world(&server);
    let greg = committed.entity_by_handle("greg").unwrap();
    assert_eq!(
        committed.as_of(greg, "hp", Tick(923)).unwrap(),
        [kala_core::Value::I64(23)]
    );
    // A reader given the last write's bookmark sees it.
    client.run(
        "MATCH (g {kala_handle:'greg'}) AT TICK 923 RETURN g.hp",
        map(&[]),
        map(&[("bookmarks", Value::List(vec![bookmark]))]),
    );
    assert_eq!(client.pull(-1).0.len(), 1);
}

#[test]
fn a_weak_mode_statement_reads_on_the_writer_not_on_the_cached_snapshot() {
    let (hook, off_mutex) = counter();
    let (server, counts) = counted_with(|config| config.after_statement_read = Some(hook));
    let mut client = Client::ready(&server);
    write(&mut client, DECLARE_HP, map(&[]));
    warm(&mut client);
    let weak = map(&[("tx_metadata", map(&[("concurrency_mode", string("weak"))]))]);
    client.run(
        "MATCH (g {kala_handle:'greg'}) SET g.hp = 1 AT TICK 901",
        map(&[]),
        weak,
    );
    assert_eq!(changed(&client.pull(-1).1), 1);
    // Weak mode checks no reads, so it never reads a snapshot that a commit
    // could have overtaken: the reading part ran under the writer mutex.
    assert_eq!(off_mutex.load(std::sync::atomic::Ordering::SeqCst), 0);
    // The same statement in the default mode reads off the mutex.
    write(
        &mut client,
        "MATCH (g {kala_handle:'greg'}) SET g.hp = 2 AT TICK 902",
        map(&[]),
    );
    assert_eq!(off_mutex.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert_eq!((counts.reader("varrow"), counts.writer("varrow")), (1, 1));
}
