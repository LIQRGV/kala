#[path = "common/client.rs"]
mod client;
mod common;

use client::{Client, map, string};
use common::{Running, VARROW};
use kala_bolt::packstream::Value;
use kala_bolt::{Server, ServerConfig};
use kala_core::{Tick, World};

fn writable_server() -> Running {
    socket_server(false, false)
}

fn socket_server(accounts: bool, read_only: bool) -> Running {
    configured_server(
        accounts,
        read_only,
        ServerConfig::default().max_message_bytes,
    )
}

fn configured_server(accounts: bool, read_only: bool, max_message_bytes: usize) -> Running {
    let dir = tempfile::tempdir().unwrap();
    drop(kala_fixture::load_fixture_to(VARROW, &dir.path().join("varrow.kala")).unwrap());
    if accounts {
        for (user, privilege) in [("reader", "read"), ("writer", "write")] {
            let account = format!("{user}@localhost");
            kala_system::system::add(dir.path(), &account, None).unwrap();
            kala_system::system::grant(dir.path(), &account, privilege, "varrow").unwrap();
        }
    }
    let config = ServerConfig {
        read_only,
        max_message_bytes,
        listen: vec!["127.0.0.1:0".parse().unwrap()],
        default_db: Some("varrow".into()),
        ..ServerConfig::default()
    };
    let server = Server::bind_accounts(dir.path(), config).unwrap();
    Running::from_server(dir, server)
}

fn world(server: &Running) -> World {
    World::open_read_only(&server.dir.path().join("varrow.kala")).unwrap()
}

fn begin(client: &mut Client) {
    client.send(0x11, vec![map(&[("db", string("varrow"))])]);
    client.success();
}

#[test]
fn set_through_a_transaction_commits_one_tx() {
    let server = writable_server();
    let before = world(&server).latest_tx();
    let mut client = Client::ready(&server);
    begin(&mut client);
    let run = client.run(
        "MATCH (c {kala_handle: $h}) SET c.hp = $hp AT TICK $t",
        map(&[
            ("h", string("greg")),
            ("hp", Value::Integer(29)),
            ("t", Value::Integer(700)),
        ]),
        map(&[]),
    );
    assert_eq!(run.get("fields"), Some(&Value::List(vec![])));
    assert!(client.pull(-1).0.is_empty());
    assert_eq!(world(&server).latest_tx(), before);
    client.send(0x12, vec![]);
    client.success();
    let committed = world(&server);
    assert_eq!(committed.latest_tx().0, before.0 + 1);
    let greg = committed.entity_by_handle("greg").unwrap();
    assert_eq!(
        committed.as_of(greg, "hp", Tick(700)).unwrap(),
        [kala_core::Value::I64(29)]
    );
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

const FORBIDDEN: &str = "Neo.ClientError.Security.Forbidden";
const CONFLICT: &str = "Neo.TransientError.Transaction.DeadlockDetected";

fn failure(client: &mut Client, query: &str, params: Value, extra: Value) -> (String, String) {
    client.send(0x10, vec![string(query), params, extra]);
    let m = client::metadata(client.recv(), 0x7f);
    let Value::String(code) = &m["code"] else {
        panic!("{m:?}")
    };
    let Value::String(message) = &m["message"] else {
        panic!("{m:?}")
    };
    client.reset();
    (code.clone(), message.clone())
}

fn commit(client: &mut Client) {
    client.send(0x12, vec![]);
    client.success();
}

#[test]
fn cypher_privileges_are_checked_for_the_whole_statement() {
    let server = socket_server(true, false);
    let mut writer = login(&server, "writer");
    writer.run_empty("CREATE (:T {kala_handle:'blind', x:1})");
    let (rows, summary) = writer.pull(-1);
    assert!(rows.is_empty());
    assert_eq!(summary["type"], string("w"));

    let mut reader = login(&server, "reader");
    let (code, reason) = failure(&mut reader, "CREATE (:T)", map(&[]), map(&[]));
    assert_eq!(code, FORBIDDEN);
    assert!(reason.contains("write"), "{reason}");
    let original = std::fs::read(server.dir.path().join("varrow.kala")).unwrap();
    for query in [
        "MATCH (n) CREATE (:T)",
        "CREATE (n:T) SET n.x = 1",
        "CREATE (n:T) REMOVE n.x",
        "CREATE (n:T) DELETE n",
        "CREATE (n:T) DETACH DELETE n",
        "CREATE (n:T) MERGE (m:T {kala_handle:'blind'})",
        "CREATE (n:T) RETURN n",
        "MERGE (n:T {kala_handle:'blind'})",
    ] {
        for explicit in [false, true] {
            if explicit {
                begin(&mut writer);
            }
            let (code, reason) = failure(&mut writer, query, map(&[]), map(&[]));
            assert_eq!(code, FORBIDDEN, "{query}: {reason}");
            assert!(reason.contains("read"), "{query}: {reason}");
        }
    }
    assert!(std::fs::read(server.dir.path().join("varrow.kala")).unwrap() == original);
}

#[test]
fn system_refuses_every_write_position() {
    let server = socket_server(true, false);
    let data = server.dir.path().join("system.kala");
    let original = std::fs::read(&data).unwrap();
    let mut admin = login(&server, "root");
    for query in [
        "CREATE (:ACCOUNT)",
        "CREATE (:ACCOUNT {password_hash:'example'})",
        "MATCH (a) SET a.password_hash = 'example'",
        "MATCH (a) SET a += {password_hash: 'example'}",
        "MATCH (a) REMOVE a.password_hash",
        "MERGE (a:ACCOUNT {kala_handle:'new'}) ON CREATE SET a.password_hash = 'example'",
        "MERGE (a:ACCOUNT {kala_handle:'root'}) ON MATCH SET a.password_hash = 'example'",
        "MATCH (a {password_hash:'example'}) DELETE a",
        "CREATE (a:ACCOUNT) RETURN a.password_hash",
    ] {
        let (code, reason) = failure(
            &mut admin,
            query,
            map(&[]),
            map(&[("db", string("system"))]),
        );
        assert_eq!(code, FORBIDDEN, "{query}: {reason}");
    }
    assert!(std::fs::read(data).unwrap() == original);
}

#[test]
fn read_only_modes_and_anonymous_server_refuse_blind_create() {
    let server = writable_server();
    let mut admin = Client::ready(&server);
    let query = "CREATE (:T {kala_handle:'blocked'})";
    let (code, _) = failure(&mut admin, query, map(&[]), map(&[("mode", string("r"))]));
    assert_eq!(
        code,
        "Neo.ClientError.Transaction.ForbiddenDueToTransactionType"
    );
    admin.send(0x11, vec![map(&[("mode", string("r"))])]);
    admin.success();
    let (code, _) = failure(&mut admin, query, map(&[]), map(&[]));
    assert_eq!(
        code,
        "Neo.ClientError.Transaction.ForbiddenDueToTransactionType"
    );

    for server in [socket_server(true, true), Running::start()] {
        let mut client = Client::ready(&server);
        let (code, _) = failure(
            &mut client,
            query,
            map(&[]),
            map(&[("db", string("varrow"))]),
        );
        assert_eq!(code, FORBIDDEN);
        assert!(world(&server).entity_by_handle("blocked").is_none());
    }
}

#[test]
fn returning_greg_tracks_changes_to_greg_only() {
    for (who, should_conflict) in [("mara", false), ("greg", true)] {
        let server = writable_server();
        let mut first = Client::ready(&server);
        let mut other = Client::ready(&server);
        begin(&mut first);
        assert_eq!(
            first.query("MATCH (n {kala_handle:'greg'}) RETURN n").len(),
            1
        );
        other.query(&format!(
            "// kala:datalog\n[:assert :{who} :title \"new\" :at 800]"
        ));
        first.query("// kala:datalog\n[:assert :wren :alias \"Written\" :at 801]");
        first.send(0x12, vec![]);
        if should_conflict {
            first.failure(CONFLICT);
        } else {
            first.success();
        }
    }
}

#[test]
fn undeclared_property_read_tracks_the_absence_of_its_name() {
    for (attribute, should_conflict) in [("unrelated", false), ("promoted", true)] {
        let server = writable_server();
        let mut first = Client::ready(&server);
        let mut other = Client::ready(&server);
        begin(&mut first);
        assert_eq!(
            first.query("MATCH (n {kala_handle:'greg'}) RETURN n.promoted"),
            vec![vec![Value::Null]]
        );
        other.query(&format!(
            "// kala:datalog\n[:assert :mara :{attribute} true :at 802]"
        ));
        first.query("// kala:datalog\n[:assert :wren :alias \"Written\" :at 803]");
        first.send(0x12, vec![]);
        if should_conflict {
            first.failure(CONFLICT);
        } else {
            first.success();
        }
    }
}

#[test]
fn cypher_optimistic_and_weak_transactions() {
    for weak in [false, true] {
        let server = writable_server();
        let mut first = Client::ready(&server);
        let mut second = Client::ready(&server);
        first.query("// kala:datalog\n[:assert :greg :points 0 :at 800]");
        begin(&mut first);
        begin(&mut second);
        if weak {
            assert!(second.query("SET CONCURRENCY_MODE = 'weak'").is_empty());
        }
        first.query("MATCH (n {kala_handle:'greg'}) SET n.points = 1 AT TICK 801");
        second.query("MATCH (n {kala_handle:'greg'}) SET n.points = 2 AT TICK 802");
        commit(&mut first);
        second.send(0x12, vec![]);
        if weak {
            second.success();
        } else {
            second.failure(CONFLICT);
        }
    }
    let server = writable_server();
    let mut client = Client::ready(&server);
    let (_, msg) = failure(
        &mut client,
        "SET CONCURRENCY_MODE = 'weak'",
        map(&[]),
        map(&[]),
    );
    assert!(
        msg.to_lowercase().contains("begin") || msg.contains("transaction"),
        "{msg}"
    );
    begin(&mut client);
    client.query("MATCH (n {kala_handle:'greg'}) RETURN n.name");
    let (_, msg) = failure(
        &mut client,
        "SET CONCURRENCY_MODE = 'optimistic'",
        map(&[]),
        map(&[]),
    );
    assert!(msg.contains("first"), "{msg}");
}

#[test]
fn an_empty_match_still_conflicts_with_an_insert_into_its_type() {
    let server = writable_server();
    let mut a = Client::ready(&server);
    let mut b = Client::ready(&server);
    a.query("// kala:datalog\n[:node-type :VISITOR]");
    begin(&mut a);
    a.query("MATCH (v:VISITOR) CREATE (:T)");
    b.query("CREATE (:VISITOR)");
    a.query("CREATE (:T)");
    a.send(0x12, vec![]);
    a.failure(CONFLICT);
}

#[test]
fn write_work_limits_name_the_limit_and_roll_back_every_row() {
    let server = writable_server();
    let before = world(&server).latest_tx();
    let mut client = Client::ready(&server);
    for (query, count, detail) in [
        ("UNWIND $rows AS i CREATE (:VISITOR)", 10001, "row"),
        (
            "UNWIND $rows AS i CREATE (:VISITOR {score:i})",
            5001,
            "primitive",
        ),
    ] {
        let rows = Value::List((0..count).map(Value::Integer).collect());
        let (_, message) = failure(&mut client, query, map(&[("rows", rows)]), map(&[]));
        assert!(
            message.replace(',', "").contains("10000") && message.to_lowercase().contains(detail),
            "{message}"
        );
        assert_eq!(
            world(&server).latest_tx(),
            before,
            "partial append on {detail} limit"
        );
    }
}

#[test]
fn set_by_type_touches_all_fixture_characters_in_one_commit() {
    let server = writable_server();
    let mut client = Client::ready(&server);
    let before = world(&server).latest_tx();
    client.run_empty("MATCH (c:CHARACTER) SET c.checked = true AT TICK 700");
    let (records, summary) = client.pull(-1);
    assert!(records.is_empty());
    assert_eq!(summary["type"], string("rw"));
    let Value::Map(kala) = &summary["kala"] else {
        panic!("{summary:?}")
    };
    assert_eq!(kala["changed"], Value::Integer(33));
    assert_eq!(world(&server).latest_tx().0, before.0 + 1);
}

#[test]
fn detach_delete_retracts_both_directions_but_preserves_identity_history_and_events() {
    let server = writable_server();
    let mut client = Client::ready(&server);
    let before = world(&server).latest_tx();
    let (_, msg) = failure(
        &mut client,
        "MATCH (n {kala_handle:'edwin'}) DELETE n AT TICK 710",
        map(&[]),
        map(&[]),
    );
    assert!(
        msg.contains("relationships") && msg.contains("710") && msg.contains("DETACH DELETE"),
        "{msg}"
    );
    assert_eq!(world(&server).latest_tx(), before);
    client.query("MATCH (n {kala_handle:'edwin'}) DETACH DELETE n AT TICK 710");
    let after = world(&server);
    let edwin = after.entity_by_handle("edwin").unwrap();
    let eldoria = after.entity_by_handle("eldoria").unwrap();
    let sword = after.entity_by_handle("sword").unwrap();
    assert!(after.as_of(edwin, "name", Tick(710)).unwrap().is_empty());
    assert!(!after.as_of(edwin, "name", Tick(709)).unwrap().is_empty());
    assert!(
        after
            .as_of(edwin, "located_in", Tick(710))
            .unwrap()
            .is_empty()
    );
    assert!(
        after
            .as_of(eldoria, "ruled_by", Tick(710))
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        after.as_of(edwin, "bought", Tick(710)).unwrap(),
        [kala_core::Value::Ref(sword)]
    );
    assert!(
        client
            .query("OPTIONAL MATCH (n {kala_handle:'absent'}) DELETE n AT TICK 710")
            .is_empty()
    );
    assert_eq!(world(&server).latest_tx(), after.latest_tx());
}

#[test]
fn auto_commit_and_explicit_return_report_node_identity_and_properties() {
    let server = writable_server();
    let mut client = Client::ready(&server);
    let run = client.run_empty("CREATE (n:CHARACTER:Guide {kala_handle:'guide',name:'Guide'}) AT TICK 800 RETURN n, n.name AS name");
    assert_eq!(
        run["fields"],
        Value::List(vec![string("n"), string("name")])
    );
    let (rows, summary) = client.pull(-1);
    assert_eq!(summary["type"], string("rw"));
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][1], string("Guide"));
    let Value::Structure(0x4e, node) = &rows[0][0] else {
        panic!("{rows:?}")
    };
    let Value::List(labels) = &node[1] else {
        panic!("{node:?}")
    };
    assert!(labels.contains(&string("CHARACTER")) && labels.contains(&string("Guide")));
    let Value::Map(properties) = &node[2] else {
        panic!("{node:?}")
    };
    assert_eq!(properties["name"], string("Guide"));
    let canonical = client.query("MATCH (n {kala_handle:'guide'}) RETURN n");
    assert_eq!(canonical[0][0], rows[0][0]);

    for expose in [false, true] {
        let mut a = Client::ready(&server);
        let mut b = Client::ready(&server);
        begin(&mut a);
        begin(&mut b);
        assert!(a.query("CREATE (:CHARACTER)").is_empty());
        let query = if expose {
            "CREATE (n:CHARACTER) RETURN n"
        } else {
            "CREATE (:CHARACTER)"
        };
        let rows = b.query(query);
        if expose {
            let Value::Structure(0x4e, n) = &rows[0][0] else {
                panic!("{rows:?}")
            };
            assert_eq!(n[1], Value::List(vec![string("CHARACTER")]));
        }
        commit(&mut a);
        b.send(0x12, vec![]);
        if expose {
            b.failure(CONFLICT);
        } else {
            b.success();
        }
    }
}

#[test]
fn row_order_conditional_merge_and_conflicting_handles() {
    let server = writable_server();
    let mut client = Client::ready(&server);
    let query = "UNWIND ['one','one','two'] AS h MERGE (n:CHARACTER {kala_handle:h}) ON CREATE SET n.name='Created' ON MATCH SET n.disguised=true AT TICK 810 RETURN h";
    assert_eq!(
        client.query(query),
        vec![
            vec![string("one")],
            vec![string("one")],
            vec![string("two")]
        ]
    );
    let check = |key: &str| format!("MATCH (n {{kala_handle:'{key}'}}) RETURN n.name, n.disguised");
    assert_eq!(
        client.query(&check("one")),
        vec![vec![string("Created"), Value::Bool(true)]]
    );
    assert_eq!(
        client.query(&check("two")),
        vec![vec![string("Created"), Value::Null]]
    );
    let original = world(&server).latest_tx();
    let (_, msg) = failure(
        &mut client,
        "MERGE (n:PLACE {kala_handle:'one'})",
        map(&[]),
        map(&[]),
    );
    assert!(msg.contains("CHARACTER") && msg.contains("one"), "{msg}");
    assert_eq!(world(&server).latest_tx(), original);
}

#[test]
fn event_and_handle_replacements_are_refused() {
    let server = writable_server();
    let before = world(&server).latest_tx();
    let mut client = Client::ready(&server);
    for item in [
        "SET n.bought = null",
        "REMOVE n.bought",
        "SET n.kala_handle = 'renamed'",
        "REMOVE n.kala_handle",
    ] {
        let (_, msg) = failure(
            &mut client,
            &format!("MATCH (n {{kala_handle:'edwin'}}) {item} AT TICK 901"),
            map(&[]),
            map(&[]),
        );
        if item.contains("bought") {
            assert!(msg.contains("event") && msg.contains("[:correct"), "{msg}");
        } else {
            assert!(msg.to_lowercase().contains("handle"), "{msg}");
        }
        assert_eq!(world(&server).latest_tx(), before);
    }
}

#[test]
fn the_read_tick_and_write_tick_can_differ() {
    let server = writable_server();
    let mut client = Client::ready(&server);
    client.query("MATCH (g {kala_handle:'greg'})-[:located_in]->(v {kala_handle:'varrow'}) AT TICK 225 SET g.stationed = true AT TICK 720");
    let w = world(&server);
    let greg = w.entity_by_handle("greg").unwrap();
    assert_eq!(
        w.as_of(greg, "stationed", Tick(720)).unwrap(),
        [kala_core::Value::Bool(true)]
    );
    assert!(w.as_of(greg, "stationed", Tick(225)).unwrap().is_empty());
}

#[test]
fn a_relationship_returned_from_an_explicit_write_includes_staged_properties() {
    let server = writable_server();
    let mut client = Client::ready(&server);
    begin(&mut client);
    let rows = client.query("MATCH (g {kala_handle:'greg'}), (m {kala_handle:'mara'}) CREATE (g)-[r:present {disguised:true}]->(m) AT TICK 900 RETURN r, r.disguised");
    let Value::Structure(0x52, fields) = &rows[0][0] else {
        panic!("{rows:?}")
    };
    assert_eq!(fields[3], string("present"));
    let Value::Map(properties) = &fields[4] else {
        panic!("{fields:?}")
    };
    assert_eq!(properties["disguised"], Value::Bool(true));
    assert_eq!(rows[0][1], Value::Bool(true));
    // Ordinary queries still read BEGIN's snapshot until COMMIT.
    let read = "MATCH (g {kala_handle:'greg'})-[r:present]->(m {kala_handle:'mara'}) AT TICK 900 RETURN r, r.disguised";
    assert!(client.query(read).is_empty());
    commit(&mut client);
    assert_eq!(client.query(read), rows);
}

#[test]
fn budgets_bound_cypher_operation_details_without_losing_totals() {
    let server = configured_server(false, false, 2048);
    let mut client = Client::ready(&server);
    let values = Value::List((0..48).map(Value::Integer).collect());
    client.run(
        "UNWIND $values AS v CREATE (:CHARACTER {hp:v}) AT TICK 903",
        map(&[("values", values)]),
        map(&[]),
    );
    let (records, summary) = client.pull(-1);
    assert!(records.is_empty());
    let Value::Map(kala) = &summary["kala"] else {
        panic!("{summary:?}")
    };
    let Value::List(ops) = &kala["ops"] else {
        panic!("{kala:?}")
    };
    let Value::Integer(omitted) = kala["ops_omitted"] else {
        panic!("{kala:?}")
    };
    assert!(omitted > 0);
    assert_eq!(ops.len() as i64 + omitted, 96);
    assert_eq!(kala["changed"], Value::Integer(48));
    for op in ops {
        let Value::Map(detail) = op else {
            panic!("{op:?}")
        };
        assert_eq!(detail["tick"], Value::Integer(903));
        assert!(matches!(detail["entries"], Value::List(_)));
    }
}

#[test]
fn supplied_parameters_have_cypher_value_semantics_and_missing_values_roll_back() {
    let server = writable_server();
    let before = world(&server).latest_tx();
    let mut client = Client::ready(&server);
    let create =
        "CREATE (n:CHARACTER {kala_handle:$h,alias:$names,name:$absent}) AT TICK $t RETURN n.alias";
    let base = [
        ("h", string("params")),
        ("names", Value::List(vec![string("One"), string("Two")])),
        ("absent", Value::Null),
    ];
    let (_, msg) = failure(&mut client, create, map(&base), map(&[]));
    assert!(
        msg.contains("t") && msg.to_lowercase().contains("param"),
        "{msg}"
    );
    let mut params = base.to_vec();
    params.push(("t", string("tomorrow")));
    let (code, msg) = failure(&mut client, create, map(&params), map(&[]));
    assert_eq!(code, "Neo.ClientError.Statement.TypeError", "{msg}");
    assert_eq!(world(&server).latest_tx(), before);
    *params.last_mut().unwrap() = ("t", Value::Integer(-9));
    client.run(create, map(&params), map(&[]));
    assert_eq!(
        client.pull(-1).0,
        vec![vec![Value::List(vec![string("One"), string("Two")])]]
    );
    let w = world(&server);
    let n = w.entity_by_handle("params").unwrap();
    assert!(w.as_of(n, "name", Tick(-9)).unwrap().is_empty());
    assert!(w.as_of(n, "alias", Tick(-10)).unwrap().is_empty());
}

#[test]
fn a_stale_reader_whose_set_is_now_a_noop_still_conflicts() {
    let server = writable_server();
    let mut reader = Client::ready(&server);
    let mut writer = Client::ready(&server);
    writer.query("// kala:datalog\n[:assert :greg :points 0 :at 950]");
    begin(&mut reader);
    let values = reader.query("// kala:datalog\n[:find ?p :where [:greg :points ?p]]");
    assert_eq!(values, vec![vec![Value::Integer(0)]]);
    let update = "// kala:datalog\n[:set :greg :points 1 :at 951]";
    writer.query(update);
    let after_write = world(&server).latest_tx();
    let rows = reader.query(update);
    assert_eq!(
        rows[0][4],
        Value::Integer(0),
        "the lazy writer already sees 1"
    );
    reader.send(0x12, vec![]);
    reader.failure(CONFLICT);
    assert_eq!(world(&server).latest_tx(), after_write);
}

#[test]
fn a_first_blind_create_keeps_the_begin_snapshot_for_later_match() {
    let server = writable_server();
    let mut a = Client::ready(&server);
    let mut b = Client::ready(&server);
    b.query("MATCH (m {kala_handle:'mara'}) SET m.disguised = false AT TICK 690");
    begin(&mut a);
    b.query("MATCH (m {kala_handle:'mara'}) SET m.disguised = true AT TICK 695");
    let after_b = world(&server).latest_tx();
    assert!(
        a.query("CREATE (:CHARACTER {kala_handle:'independent'})")
            .is_empty()
    );
    let read = "MATCH (m {kala_handle:'mara'}) AT TICK 700 RETURN m.disguised";
    assert_eq!(
        a.query(read),
        vec![vec![Value::Bool(false)]],
        "MATCH must read BEGIN"
    );
    assert_eq!(b.query(read), vec![vec![Value::Bool(true)]]);
    a.send(0x12, vec![]);
    a.failure(CONFLICT);
    let committed = world(&server);
    assert_eq!(committed.latest_tx(), after_b);
    assert!(committed.entity_by_handle("independent").is_none());
}

#[test]
fn a_snapshot_read_after_a_blind_create_conflicts_with_a_renamed_attribute() {
    let server = writable_server();
    let mut a = Client::ready(&server);
    let mut admin = Client::ready(&server);
    admin.query("MATCH (m {kala_handle:'mara'}) SET m.disguised = true AT TICK 690");
    begin(&mut a);
    admin.query("// kala:datalog\n[:rename :disguised :new_disguised]");
    let renamed = world(&server).latest_tx();
    assert!(
        a.query("CREATE (:CHARACTER {kala_handle:'independent'})")
            .is_empty()
    );
    let read = "MATCH (m {kala_handle:'mara'}) AT TICK 700 RETURN m.disguised";
    assert_eq!(
        a.query(read),
        vec![vec![Value::Bool(true)]],
        "pinned BEGIN schema and value"
    );
    assert_eq!(
        admin.query(read),
        vec![vec![Value::Null]],
        "old name has vanished"
    );
    a.send(0x12, vec![]);
    a.failure(CONFLICT);
    let committed = world(&server);
    assert_eq!(committed.latest_tx(), renamed);
    assert!(committed.entity_by_handle("independent").is_none());
}

#[path = "../../kala-lang/tests/common/dotted_names.rs"]
mod dotted_names;

#[test]
fn dotted_names_through_bolt_preserve_the_world() {
    let server = writable_server();
    let mut client = Client::ready(&server);
    let path = server.dir.path().join("varrow.kala");
    let before = std::fs::read(&path).unwrap();
    for &(lang, name, query) in dotted_names::WRITES {
        let query = format!("// kala:{lang}\n{query}");
        let (code, reason) = failure(&mut client, &query, map(&[]), map(&[]));
        assert!(code.starts_with("Neo.ClientError."), "{code}: {reason}");
        assert!(
            reason.contains(name) && reason.contains("may not contain '.'"),
            "{query}: {reason}"
        );
        assert!(std::fs::read(&path).unwrap() == before, "{query}");
    }
    let datalog_read = format!("// kala:datalog\n{}", dotted_names::DATALOG_READ);
    let (_, reason) = failure(&mut client, &datalog_read, map(&[]), map(&[]));
    assert!(reason.contains("unknown attribute `:a.b`"), "{reason}");
    assert_eq!(
        client.query(dotted_names::CYPHER_READ),
        vec![vec![Value::Null]]
    );
}

fn supplied_rows(rows: Vec<Value>) -> Value {
    map(&[("rows", Value::List(rows))])
}

#[test]
fn bulk_params_map_keys_are_input_data_and_missing_keys_are_null() {
    let server = writable_server();
    let mut c = Client::ready(&server);
    let rows = vec![
        map(&[
            ("password_hash", string("client data")),
            (
                "aliases",
                Value::List(vec![string("Sparrow"), string("Scout")]),
            ),
        ]),
        map(&[("password_hash", Value::Null)]),
    ];
    c.run(
        "UNWIND $rows AS row WITH * RETURN row.password_hash, row.aliases",
        supplied_rows(rows.clone()),
        map(&[("db", string("system"))]),
    );
    assert_eq!(
        c.pull(-1).0,
        vec![
            vec![
                string("client data"),
                Value::List(vec![string("Sparrow"), string("Scout")])
            ],
            vec![Value::Null, Value::Null],
        ]
    );
    let (code, _) = failure(
        &mut c,
        "UNWIND $rows AS row MATCH (n) RETURN row.password_hash, n.password_hash",
        supplied_rows(rows),
        map(&[("db", string("system"))]),
    );
    assert_eq!(code, FORBIDDEN);
}

#[test]
fn bulk_params_map_positions_and_inconsistent_columns_fail_before_commit() {
    let server = writable_server();
    let mut c = Client::ready(&server);
    let before = world(&server).latest_tx();
    let row = map(&[("name", string("marker-never-echo"))]);
    let write = "UNWIND $rows AS row CREATE (:NPC {name:row.name})";
    for (query, params, expected) in [
        (
            "WITH $rows AS x RETURN x",
            supplied_rows(vec![row.clone()]),
            "unwind",
        ),
        (
            "UNWIND $rows AS row WITH * RETURN row",
            supplied_rows(vec![row.clone()]),
            "row",
        ),
        (
            "UNWIND $rows AS row RETURN size(row)",
            supplied_rows(vec![row.clone()]),
            "row",
        ),
        (
            "UNWIND $rows AS row WITH * WHERE row = row RETURN row.name",
            supplied_rows(vec![row.clone()]),
            "row",
        ),
        (
            "UNWIND $rows AS row CREATE (:NPC {name:row})",
            supplied_rows(vec![row.clone()]),
            "row",
        ),
        (
            "UNWIND $rows AS row CREATE (n:NPC) SET n += row",
            supplied_rows(vec![row.clone()]),
            "map",
        ),
        (write, map(&[("rows", row.clone())]), "map"),
        (
            write,
            supplied_rows(vec![map(&[("name", row.clone())])]),
            "map",
        ),
        (
            write,
            supplied_rows(vec![row.clone(), map(&[("name", Value::Integer(5))])]),
            "row 1",
        ),
    ] {
        let (_, message) = failure(&mut c, query, params, map(&[]));
        assert!(
            message.to_ascii_lowercase().contains(expected),
            "{query}: {message}"
        );
        if expected == "row 1" {
            assert!(message.contains("name"), "{message}");
        }
        assert!(
            !message.contains("marker-never-echo"),
            "{query} echoed a parameter value"
        );
        assert_eq!(world(&server).latest_tx(), before, "{query}");
    }
}

#[test]
fn bulk_params_datalog_reports_missing_and_wrong_typed_bindings() {
    let server = writable_server();
    let mut c = Client::ready(&server);
    let before = world(&server).latest_tx();
    let stmt = "// kala:datalog\n[:assert ?e :name ?v :at ?t]";
    for (params, code) in [
        (
            map(&[("e", string("greg")), ("v", string("Scout"))]),
            "Neo.ClientError.Statement.ParameterMissing",
        ),
        (
            map(&[
                ("e", string("greg")),
                ("v", Value::Null),
                ("t", Value::Integer(-4)),
            ]),
            "Neo.ClientError.Statement.TypeError",
        ),
        (
            map(&[
                ("e", string("greg")),
                ("v", Value::List(vec![string("Scout")])),
                ("t", Value::Integer(-4)),
            ]),
            "Neo.ClientError.Statement.TypeError",
        ),
        (
            map(&[
                ("e", string("greg")),
                ("v", string("Scout")),
                ("t", string("marker-never-echo")),
            ]),
            "Neo.ClientError.Statement.TypeError",
        ),
    ] {
        let (actual, message) = failure(&mut c, stmt, params, map(&[]));
        assert_eq!(actual, code, "{message}");
        assert!(!message.contains("marker-never-echo"));
        assert_eq!(world(&server).latest_tx(), before);
    }
    c.run(
        stmt,
        map(&[
            ("e", string("greg")),
            ("v", string("Scout")),
            ("t", Value::Integer(-4)),
        ]),
        map(&[]),
    );
    c.pull(-1);
    let w = world(&server);
    assert!(
        w.as_of(w.entity_by_handle("greg").unwrap(), "name", Tick(-4))
            .unwrap()
            .contains(&kala_core::Value::Str("Scout".into()))
    );
}

#[test]
fn bulk_params_null_tick_uses_the_statement_default_and_wrong_tick_identifies_the_row() {
    let server = writable_server();
    let mut c = Client::ready(&server);
    let w = world(&server);
    let before = w.latest_tx();
    let default = w.latest_tick();
    let write =
        "UNWIND $rows AS row CREATE (:NPC {kala_handle:row.handle, score:7}) AT TICK row.tick";
    let row = |handle: &str, tick| map(&[("handle", string(handle)), ("tick", tick)]);
    let (_, message) = failure(
        &mut c,
        write,
        supplied_rows(vec![
            row("bulk_early", Value::Null),
            row("bulk_bad", string("marker-never-echo")),
        ]),
        map(&[]),
    );
    assert!(
        message.contains("row 1") && message.to_ascii_lowercase().contains("tick"),
        "{message}"
    );
    assert!(!message.contains("marker-never-echo"));
    assert_eq!(world(&server).latest_tx(), before);
    c.run(
        write,
        supplied_rows(vec![
            row("bulk_early", Value::Integer(i64::MIN)),
            row("bulk_default", Value::Null),
            row("bulk_late", Value::Integer(i64::MAX)),
        ]),
        map(&[]),
    );
    let (_, summary) = c.pull(-1);
    let Value::Map(detail) = &summary["kala"] else {
        panic!("expected the write summary");
    };
    assert_eq!(detail["tick_min"], Value::Integer(i64::MIN));
    assert_eq!(detail["tick_max"], Value::Integer(i64::MAX));
    assert_eq!(detail["tick_count"], Value::Integer(6));
    let w = world(&server);
    for (handle, tick) in [
        ("bulk_early", Tick(i64::MIN)),
        ("bulk_default", default),
        ("bulk_late", Tick(i64::MAX)),
    ] {
        let e = w.entity_by_handle(handle).unwrap();
        assert_eq!(w.history(e, "score").unwrap()[0].tick, tick);
    }
}

#[test]
fn bulk_params_per_row_write_preserves_optimistic_read_conflicts() {
    let server = writable_server();
    let mut a = Client::ready(&server);
    let mut b = Client::ready(&server);
    begin(&mut a);
    a.query("MATCH (n {kala_handle:'greg'}) RETURN n.name");
    b.query("MATCH (n {kala_handle:'greg'}) SET n.name='Captain' AT TICK 2000");
    let committed = world(&server).latest_tx();
    a.run(
        "UNWIND $rows AS row CREATE (:NPC {kala_handle:row.handle,score:2}) AT TICK row.tick",
        supplied_rows(vec![map(&[
            ("handle", string("stale")),
            ("tick", Value::Integer(0)),
        ])]),
        map(&[]),
    );
    a.pull(-1);
    a.send(0x12, vec![]);
    a.failure(CONFLICT);
    assert_eq!(world(&server).latest_tx(), committed);
    assert!(world(&server).entity_by_handle("stale").is_none());
}

#[test]
fn bulk_params_datalog_counts_bound_strings_toward_the_transaction_budget() {
    let server = writable_server();
    let mut c = Client::ready(&server);
    c.query("// kala:datalog\n[:attribute :bulk_note :string :unindexed]");
    let before = world(&server).latest_tx();
    begin(&mut c);
    // Repeating one assertion is a no-op after the first. Its supplied bytes
    // still belong to each RUN, and all the RUNs share a single 16 MiB limit.
    let params = map(&[("note", Value::String("x".repeat(512 * 1024)))]);
    let mut refused = false;
    for attempt in 0..33 {
        c.send(
            0x10,
            vec![
                string("// kala:datalog\n[:assert :greg :bulk_note ?note]"),
                params.clone(),
                map(&[]),
            ],
        );
        let response = c.recv();
        if matches!(response, Value::Structure(0x7f, _)) {
            let m = client::metadata(response, 0x7f);
            let Value::String(message) = &m["message"] else {
                panic!("expected failure detail");
            };
            assert!(attempt > 0, "the first write fits in the budget: {message}");
            assert!(message.contains("byte limit"), "{message}");
            refused = true;
            c.reset();
            break;
        }
        client::metadata(response, 0x70);
        c.pull(-1);
    }
    assert!(
        refused,
        "transaction accepted more than 16 MiB in parameters"
    );
    assert_eq!(world(&server).latest_tx(), before);
}

#[test]
fn bulk_params_returned_nodes_and_properties_use_each_rows_tick() {
    for explicit in [true, false] {
        let server = writable_server();
        let mut c = Client::ready(&server);
        if explicit {
            begin(&mut c);
        }
        let rows = [(8, "Scout"), (32, "Guide")].map(|(tick, name)| {
            map(&[
                ("names", Value::List(vec![string(name)])),
                ("tick", Value::Integer(tick)),
            ])
        });
        c.run("UNWIND $rows AS row MERGE (n:NPC {kala_handle:'wren_two_ticks'}) SET n.name=row.names[0] AT TICK row.tick RETURN n, n.name",
            supplied_rows(rows.to_vec()), map(&[]));
        let (written, _) = c.pull(-1);
        assert_eq!(written.len(), 2);
        for (row, want) in written.iter().zip(["Scout", "Guide"]) {
            let Value::Structure(0x4e, node) = &row[0] else {
                panic!("expected a node");
            };
            let Value::Map(properties) = &node[2] else {
                panic!("expected node properties");
            };
            assert_eq!(properties["name"], string(want));
            assert_eq!(row[1], string(want));
        }
        if explicit {
            commit(&mut c);
        }
    }
}

#[test]
fn bulk_params_duplicate_handle_is_refused_without_echoing_the_supplied_value() {
    let server = writable_server();
    let mut c = Client::ready(&server);
    let before = world(&server).latest_tx();
    let row = map(&[("handle", string("client_only_marker"))]);
    let (_, message) = failure(
        &mut c,
        "UNWIND $rows AS row CREATE (:NPC {kala_handle:row.handle})",
        supplied_rows(vec![row.clone(), row]),
        map(&[]),
    );
    assert!(!message.contains("client_only_marker"));
    assert_eq!(world(&server).latest_tx(), before);
}

#[test]
fn bulk_params_blind_write_reports_schema_type_failures_like_literal_writes() {
    let server = socket_server(true, false);
    let mut c = login(&server, "writer");
    let literal = failure(
        &mut c,
        "// kala:datalog\n[:assert :greg :name 37]",
        map(&[]),
        map(&[]),
    );
    assert_eq!(literal.0, "Neo.ClientError.Statement.ExecutionFailed");
    for explicit in [false, true] {
        if explicit {
            begin(&mut c);
        }
        let parameterized = failure(
            &mut c,
            "// kala:datalog\n[:assert :greg :name ?v]",
            map(&[("v", Value::Integer(37))]),
            map(&[]),
        );
        assert_eq!(parameterized, literal);
    }
}

#[test]
fn bulk_params_wire_map_rows_over_the_limit_allow_reset_and_reuse() {
    let server = writable_server();
    let mut c = Client::ready(&server);
    let before = world(&server).latest_tx();
    let row = map(&[("tick", Value::Integer(0))]);
    let (code, message) = failure(
        &mut c,
        "UNWIND $rows AS row CREATE (:NPC) AT TICK row.tick",
        supplied_rows(vec![row; 10_001]),
        map(&[]),
    );
    assert_eq!(code, "Neo.ClientError.Statement.TypeError", "{message}");
    assert!(
        message.replace(',', "").contains("10000") && message.contains("row"),
        "{message}"
    );
    assert_eq!(world(&server).latest_tx(), before);
    assert_eq!(
        c.query("WITH 1 AS x RETURN x"),
        vec![vec![Value::Integer(1)]]
    );
}

#[test]
fn bulk_params_binding_error_names_the_parameter_and_kind_never_the_value() {
    let server = writable_server();
    let mut c = Client::ready(&server);
    for (text, name, kind) in [
        (
            "MATCH (n:CHARACTER) WHERE n.located_in = $place RETURN count(*)",
            "`$place`",
            "an entity",
        ),
        (
            "// kala:datalog\n[:assert :greg :alias \"G\" :at ?place]",
            "`?place`",
            "a tick",
        ),
        (
            "// kala:datalog\n[:find ?e :in ?place :where [?e :located_in ?place]]",
            "`?place`",
            "an entity",
        ),
    ] {
        let (code, message) = failure(
            &mut c,
            text,
            map(&[("place", string("client_only_marker"))]),
            map(&[]),
        );
        assert_eq!(
            code, "Neo.ClientError.Statement.TypeError",
            "{text}: {message}"
        );
        assert!(
            message.contains(name) && message.contains(kind),
            "{text}: {message}"
        );
        assert!(!message.contains("client_only_marker"), "{text}: {message}");
    }
}

#[test]
fn bulk_params_datalog_parameter_types_follow_declarations_staged_or_committed_just_before() {
    let server = writable_server();
    let mut c = Client::ready(&server);
    let params = || {
        map(&[
            ("e", string("greg")),
            ("w", Value::Integer(80)),
            ("to", string("edwin")),
        ])
    };
    // An explicit transaction: the declaration is only staged when the
    // parameterized form is bound.
    begin(&mut c);
    c.run(
        "// kala:datalog\n[:attribute :weight :f64]",
        map(&[]),
        map(&[]),
    );
    c.pull(-1);
    c.run(
        "// kala:datalog\n[:attribute :mentor_of :ref]",
        map(&[]),
        map(&[]),
    );
    c.pull(-1);
    c.run(
        "// kala:datalog\n[[:assert ?e :weight ?w :at 5] [:assert ?e :mentor_of ?to :at 5]]",
        params(),
        map(&[]),
    );
    c.pull(-1);
    commit(&mut c);
    // One auto-commit RUN: each form is its own transaction, bound after
    // the forms before it committed.
    c.run(
        "// kala:datalog\n[:attribute :weight3 :f64] [:attribute :pupil_of :ref] [[:assert ?e :weight3 ?w :at 5] [:assert ?to :pupil_of ?e :at 5]]",
        params(),
        map(&[]),
    );
    c.pull(-1);
    let w = world(&server);
    let greg = w.entity_by_handle("greg").unwrap();
    let edwin = w.entity_by_handle("edwin").unwrap();
    for attr in ["weight", "weight3"] {
        assert_eq!(
            w.as_of(greg, attr, Tick(5)).unwrap(),
            [kala_core::Value::f64(80.0).unwrap()],
            "{attr}"
        );
    }
    assert_eq!(
        w.as_of(greg, "mentor_of", Tick(5)).unwrap(),
        [kala_core::Value::Ref(edwin)]
    );
    assert_eq!(
        w.as_of(edwin, "pupil_of", Tick(5)).unwrap(),
        [kala_core::Value::Ref(greg)]
    );
}

#[test]
fn bulk_params_string_parameter_for_an_undeclared_attribute_is_refused() {
    let server = writable_server();
    let mut c = Client::ready(&server);
    let before = world(&server).latest_tx();
    let pair = || map(&[("a", string("greg")), ("b", string("edwin"))]);
    for explicit in [false, true] {
        if explicit {
            begin(&mut c);
        }
        let (code, message) = failure(
            &mut c,
            "// kala:datalog\n[:assert ?a :friend_of ?b :at 700]",
            pair(),
            map(&[]),
        );
        assert_eq!(code, "Neo.ClientError.Statement.TypeError", "{message}");
        for part in [
            ":friend_of",
            "[:attribute :friend_of :ref]",
            ":string",
            "?b",
        ] {
            assert!(message.contains(part), "missing {part}: {message}");
        }
        assert!(!message.contains("edwin"), "{message}");
        let w = world(&server);
        assert_eq!(w.latest_tx(), before);
        assert!(w.attribute("friend_of").is_none());
    }
    // Declared first, the same form interns an edge, as the literal form does.
    c.query("// kala:datalog\n[:attribute :friend_of :ref]");
    c.run(
        "// kala:datalog\n[:assert ?a :friend_of ?b :at 700]",
        pair(),
        map(&[]),
    );
    c.pull(-1);
    let w = world(&server);
    let (greg, edwin) = (
        w.entity_by_handle("greg").unwrap(),
        w.entity_by_handle("edwin").unwrap(),
    );
    assert_eq!(
        w.as_of(greg, "friend_of", Tick(700)).unwrap(),
        [kala_core::Value::Ref(edwin)]
    );
    // A number still declares the attribute by inference, as a literal would.
    c.run(
        "// kala:datalog\n[:assert ?a :shoe_size ?n :at 700]",
        map(&[("a", string("greg")), ("n", Value::Integer(43))]),
        map(&[]),
    );
    c.pull(-1);
    assert_eq!(
        world(&server).attribute("shoe_size").unwrap().value_type,
        kala_core::ValueType::I64
    );
}
