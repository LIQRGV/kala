#[path = "common/client.rs"]
mod client;
mod common;

use client::{Client, map, string};
use common::{Running, VARROW};
use kala_bolt::packstream::Value;
use kala_bolt::{Server, ServerConfig};
use kala_core::World;

const WRITE: &str = "// kala:datalog\n[:assert :greg :alias \"Ned\" :at 300]";

fn writable_server() -> Running {
    writable_server_with(ServerConfig::default())
}

fn writable_server_with(mut config: ServerConfig) -> Running {
    let dir = tempfile::tempdir().unwrap();
    drop(kala_fixture::load_fixture_to(VARROW, &dir.path().join("varrow.kala")).unwrap());
    config.listen = vec!["127.0.0.1:0".parse().unwrap()];
    config.default_db = Some("varrow".into());
    let server = Server::bind_accounts(dir.path(), config).unwrap();
    Running::from_server(dir, server)
}

fn world(server: &Running) -> World {
    World::open_read_only(&server.dir.path().join("varrow.kala")).unwrap()
}

#[test]
fn begin_run_commit_writes_one_transaction() {
    let server = writable_server();
    let before = world(&server).latest_tx();
    let mut client = Client::ready(&server);
    client.send(0x11, vec![map(&[("db", string("varrow"))])]);
    client.success();
    client.run_empty(WRITE);
    let (rows, _) = client.pull(-1);
    assert_eq!(rows.len(), 1);
    assert_eq!(world(&server).latest_tx(), before, "RUN must only stage");
    client.send(0x12, vec![]);
    client.success();
    let after = world(&server);
    assert_eq!(after.latest_tx().0, before.0 + 1);
    let greg = after.entity_by_handle("greg").unwrap();
    assert!(
        after
            .as_of(greg, "alias", kala_core::Tick(300))
            .unwrap()
            .contains(&kala_core::Value::Str("Ned".into()))
    );
}

#[test]
fn auto_commit_write_commits_before_run_succeeds() {
    let server = writable_server();
    let before = world(&server).latest_tx();
    let mut client = Client::ready(&server);
    let run = client.run_empty(WRITE);
    assert_eq!(
        run.get("fields"),
        Some(&Value::List(
            [
                "form", "op", "kind", "tick", "changed", "entries", "entity", "created"
            ]
            .into_iter()
            .map(string)
            .collect()
        ))
    );
    let after = world(&server);
    assert_eq!(
        after.latest_tx().0,
        before.0 + 1,
        "RUN success must follow durable commit even without PULL"
    );
    let greg = after.entity_by_handle("greg").unwrap();
    assert!(
        after
            .as_of(greg, "alias", kala_core::Tick(300))
            .unwrap()
            .contains(&kala_core::Value::Str("Ned".into()))
    );
    let mut reader = Client::ready(&server);
    assert!(
        reader
            .query("// kala:datalog\n[:find ?a :at 300 :where [:greg :alias ?a]]")
            .contains(&vec![string("Ned")])
    );
}

const INVALID: &str = "Neo.ClientError.Request.Invalid";
const FORBIDDEN: &str = "Neo.ClientError.Security.Forbidden";
const CONFLICT: &str = "Neo.TransientError.Transaction.DeadlockDetected";
const ACCESS_MODE: &str = "Neo.ClientError.Transaction.ForbiddenDueToTransactionType";
const ALIASES: &str = "// kala:datalog\n[:find ?a :at 300 :where [:greg :alias ?a]]";

fn begin<S: std::io::Read + std::io::Write>(client: &mut Client<S>) {
    client.send(0x11, vec![map(&[("db", string("varrow"))])]);
    client.success();
}

fn write<S: std::io::Read + std::io::Write>(
    client: &mut Client<S>,
    body: &str,
) -> (Vec<Vec<Value>>, std::collections::BTreeMap<String, Value>) {
    client.run_empty(&format!("// kala:datalog\n{body}"));
    client.pull(-1)
}

fn commit<S: std::io::Read + std::io::Write>(
    client: &mut Client<S>,
) -> std::collections::BTreeMap<String, Value> {
    client.send(0x12, vec![]);
    client.success()
}

fn values(server: &Running, handle: &str, attr: &str, tick: i64) -> Vec<kala_core::Value> {
    let w = world(server);
    w.as_of(
        w.entity_by_handle(handle).unwrap(),
        attr,
        kala_core::Tick(tick),
    )
    .unwrap()
}

fn failure_text(meta: &std::collections::BTreeMap<String, Value>) -> &str {
    let Some(Value::String(text)) = meta.get("message") else {
        panic!("missing failure message: {meta:?}")
    };
    text
}

#[test]
fn explicit_multiple_runs_share_one_commit_and_provisional_entries_are_empty() {
    let server = writable_server();
    let before = world(&server).latest_tx();
    let mut client = Client::ready(&server);
    begin(&mut client);
    let (created, _) = write(&mut client, "[:create :CHARACTER :new_character]");
    assert_eq!(created.len(), 1);
    assert_eq!(created[0][0], Value::Integer(1));
    assert_eq!(created[0][2], string("create"));
    assert_eq!(created[0][5], Value::List(vec![]));
    assert!(matches!(created[0][6], Value::Structure(0x4e, _)));
    assert_eq!(created[0][7], Value::Bool(true));
    let (asserted, _) = write(
        &mut client,
        "[:assert :new_character :alias \"Created here\" :at -9]",
    );
    assert_eq!(asserted[0][5], Value::List(vec![]));
    assert_eq!(world(&server).latest_tx(), before);
    assert!(world(&server).entity_by_handle("new_character").is_none());
    let summary = commit(&mut client);
    assert_eq!(summary.get("type"), Some(&string("w")));
    let Value::Map(kala) = &summary["kala"] else {
        panic!("missing Kala summary")
    };
    assert!(matches!(&kala["entries"], Value::List(ids) if !ids.is_empty()));
    assert_eq!(world(&server).latest_tx().0, before.0 + 1);
    assert_eq!(
        values(&server, "new_character", "alias", -9),
        vec![kala_core::Value::Str("Created here".into())]
    );
}

#[test]
fn rollback_reset_failure_and_disconnect_leave_no_writes() {
    let server = writable_server();
    let original = std::fs::read(server.dir.path().join("varrow.kala")).unwrap();
    for ending in [0x13, 0x0f, 0x02] {
        let mut client = Client::ready(&server);
        begin(&mut client);
        write(&mut client, "[:create :CHARACTER :never_committed]");
        client.send(ending, vec![]);
        if ending == 0x02 {
            common::assert_closed(&mut client.stream);
        } else {
            client.success();
            assert_eq!(client.query(client::ALLIES).len(), 2);
        }
        assert_eq!(
            std::fs::read(server.dir.path().join("varrow.kala")).unwrap(),
            original
        );
    }
    let mut client = Client::ready(&server);
    begin(&mut client);
    write(&mut client, "[:create :CHARACTER :never_committed]");
    client.send(
        0x10,
        vec![
            string("// kala:datalog\n[:assert :missing_handle :alias \"x\"]"),
            map(&[]),
            map(&[]),
        ],
    );
    let failure = client::metadata(client.recv(), 0x7f);
    assert!(
        matches!(failure.get("code"), Some(Value::String(code)) if code.starts_with("Neo.ClientError.Statement."))
    );
    client.send(0x12, vec![]);
    assert!(client::structure(client.recv(), 0x7e).is_empty());
    client.reset();
    assert_eq!(
        std::fs::read(server.dir.path().join("varrow.kala")).unwrap(),
        original
    );
    let mut dropped = Client::ready(&server);
    begin(&mut dropped);
    write(&mut dropped, "[:create :CHARACTER :dropped]");
    drop(dropped);
    assert!(world(&server).entity_by_handle("dropped").is_none());
}

#[test]
fn transaction_state_machine_refuses_nested_begin_or_commit_with_open_result() {
    let server = writable_server();
    let mut client = Client::ready(&server);
    for tag in [0x12, 0x13] {
        client.send(tag, vec![]);
        client.failure(INVALID);
        client.reset();
    }
    for tag in [0x11, 0x12, 0x13] {
        begin(&mut client);
        if tag != 0x11 {
            client.run_empty(client::NAMES);
        }
        client.send(tag, if tag == 0x11 { vec![map(&[])] } else { vec![] });
        client.failure(INVALID);
        client.reset();
        assert_eq!(client.query(client::ALLIES).len(), 2);
    }
    begin(&mut client);
    client.run_empty(client::NAMES);
    assert_eq!(client.pull(1).0.len(), 1);
    client.send(0x2f, vec![map(&[("n", Value::Integer(-1))])]);
    client.success();
    commit(&mut client);
    assert_eq!(client.query(client::ALLIES).len(), 2);
}

#[test]
fn auto_commit_report_has_exact_shape_bookmark_and_honest_counters() {
    let server = writable_server();
    let mut client = Client::ready(&server);
    let before = world(&server).latest_tx().0;
    let (rows, summary) = write(&mut client, "[:assert :greg :alias \"Ned\" :at 300]");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].len(), 8);
    assert_eq!(
        &rows[0][..5],
        &[
            Value::Integer(1),
            Value::Integer(1),
            string("assert"),
            Value::Integer(300),
            Value::Integer(1)
        ]
    );
    assert!(
        matches!(&rows[0][5], Value::List(ids) if ids.len() == 1 && matches!(ids[0], Value::Integer(_)))
    );
    assert_eq!(rows[0][6], Value::Null);
    assert_eq!(rows[0][7], Value::Null);
    assert_eq!(summary["type"], string("w"));
    assert_eq!(summary["db"], string("varrow"));
    assert_eq!(
        summary["bookmark"],
        string(&format!("kala:varrow:{}", before + 1))
    );
    let Value::Map(stats) = &summary["stats"] else {
        panic!("stats must be a map")
    };
    assert_eq!(stats.get("properties-set"), Some(&Value::Integer(1)));
    assert_eq!(stats.get("contains-updates"), Some(&Value::Bool(true)));
    let Value::Map(kala) = &summary["kala"] else {
        panic!("kala must be a map")
    };
    assert_eq!(kala["tx"], Value::Integer((before + 1) as i64));
    assert_eq!(kala["changed"], Value::Integer(1));
    assert_eq!(kala["declared"], Value::Integer(0));
    assert_eq!(kala["entries"], rows[0][5]);
    let w = world(&server);
    let report = kala_core::doctor::doctor(
        &w,
        &kala_core::doctor::DoctorOptions {
            since: Some(kala_core::TxId(before)),
            ..Default::default()
        },
    );
    assert!(
        report.is_clean(),
        "new Bolt write should pass doctor: {report:?}"
    );
}

#[test]
fn multi_form_auto_commit_stops_after_failure_and_preserves_prior_commits() {
    let server = writable_server();
    let before = world(&server).latest_tx().0;
    let mut client = Client::ready(&server);
    client.send(0x10, vec![string("// kala:datalog\n[:assert :greg :alias \"First\" :at 300]\n[:assert :missing_handle :alias \"Second\" :at 300]\n[:assert :greg :alias \"Third\" :at 300]"), map(&[]), map(&[])]);
    let failure = client::metadata(client.recv(), 0x7f);
    let text = failure_text(&failure);
    assert!(text.contains("form 2 failed"), "{text}");
    assert!(text.contains("1 prior forms committed"), "{text}");
    assert_eq!(world(&server).latest_tx().0, before + 1);
    let held = values(&server, "greg", "alias", 300);
    assert!(held.contains(&kala_core::Value::Str("First".into())));
    assert!(!held.contains(&kala_core::Value::Str("Third".into())));
    client.reset();
    assert_eq!(client.query(client::ALLIES).len(), 2);
}

#[test]
fn multiple_forms_inside_explicit_transaction_commit_atomically() {
    let server = writable_server();
    let before = world(&server).latest_tx().0;
    let mut client = Client::ready(&server);
    begin(&mut client);
    let (rows, _) = write(
        &mut client,
        "[:create :CHARACTER :made_here]\n[:assert :made_here :alias \"Together\" :at 300]",
    );
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0][0], Value::Integer(1));
    assert_eq!(rows[1][0], Value::Integer(2));
    commit(&mut client);
    assert_eq!(world(&server).latest_tx().0, before + 1);
    assert_eq!(
        values(&server, "made_here", "alias", 300),
        vec![kala_core::Value::Str("Together".into())]
    );
}

#[test]
fn queries_keep_begin_snapshot_and_do_not_see_own_writes() {
    let server = writable_server();
    let mut a = Client::ready(&server);
    let mut b = Client::ready(&server);
    begin(&mut a);
    let before = a.query(ALIASES);
    write(
        &mut b,
        "[:assert :greg :alias \"Other connection\" :at 300]",
    );
    assert_eq!(a.query(ALIASES), before);
    write(
        &mut a,
        "[:assert :greg :alias \"Own staged value\" :at 300]",
    );
    assert_eq!(a.query(ALIASES), before);
    a.send(0x13, vec![]);
    a.success();
    assert!(a.query(ALIASES).contains(&vec![string("Other connection")]));
    assert!(!a.query(ALIASES).contains(&vec![string("Own staged value")]));
}

#[test]
fn blind_writes_from_two_snapshots_both_commit() {
    let server = writable_server();
    let mut a = Client::ready(&server);
    let mut b = Client::ready(&server);
    begin(&mut a);
    begin(&mut b);
    write(&mut a, "[:assert :greg :alias \"A\" :at 300]");
    write(&mut b, "[:assert :greg :alias \"B\" :at 300]");
    commit(&mut a);
    commit(&mut b);
    let held = values(&server, "greg", "alias", 300);
    for name in ["A", "B"] {
        assert!(held.contains(&kala_core::Value::Str(name.into())));
    }
}

#[test]
fn conflicting_sets_are_transient_and_failed_commit_writes_nothing() {
    let server = writable_server();
    let before = world(&server).latest_tx().0;
    let mut a = Client::ready(&server);
    let mut b = Client::ready(&server);
    begin(&mut a);
    begin(&mut b);
    write(&mut a, "[:set :greg :located_in :carnath :at 700]");
    write(&mut b, "[:set :greg :located_in :varrow :at 701]");
    commit(&mut a);
    b.send(0x12, vec![]);
    let failure = b.failure(CONFLICT);
    assert!(!failure_text(&failure).contains("ashford"));
    assert_eq!(world(&server).latest_tx().0, before + 1);
    b.reset();
    assert_eq!(b.query(client::ALLIES).len(), 2);
}

#[test]
fn weak_concurrency_metadata_and_first_form_allow_competing_sets() {
    for form_mode in [false, true] {
        let server = writable_server();
        let mut a = Client::ready(&server);
        let mut b = Client::ready(&server);
        for client in [&mut a, &mut b] {
            let extra = if form_mode {
                map(&[])
            } else {
                map(&[("tx_metadata", map(&[("concurrency_mode", string("weak"))]))])
            };
            client.send(0x11, vec![extra]);
            client.success();
        }
        let prefix = if form_mode {
            "[[:concurrency :weak] "
        } else {
            "["
        };
        write(
            &mut a,
            &format!("{prefix}[:set :greg :located_in :carnath :at 700]]"),
        );
        write(
            &mut b,
            &format!("{prefix}[:set :greg :located_in :varrow :at 701]]"),
        );
        commit(&mut a);
        commit(&mut b);
    }
}

#[test]
fn concurrency_directive_is_refused_after_first_write_form() {
    let server = writable_server();
    let mut client = Client::ready(&server);
    begin(&mut client);
    write(&mut client, "[:assert :greg :alias \"First\" :at 300]");
    client.send(
        0x10,
        vec![
            string(
                "// kala:datalog\n[[:concurrency :weak] [:assert :greg :alias \"Second\" :at 300]]",
            ),
            map(&[]),
            map(&[]),
        ],
    );
    let failure = client::metadata(client.recv(), 0x7f);
    assert!(failure_text(&failure).contains("first"));
    client.reset();
    assert!(
        !values(&server, "greg", "alias", 300).contains(&kala_core::Value::Str("First".into()))
    );
}

#[test]
fn exposed_provisional_node_ids_turn_renumbering_into_conflict() {
    let server = writable_server();
    let mut a = Client::ready(&server);
    let mut b = Client::ready(&server);
    begin(&mut a);
    begin(&mut b);
    let (arows, _) = write(&mut a, "[:create :CHARACTER :allocated_a]");
    let (brows, _) = write(&mut b, "[:create :CHARACTER :allocated_b]");
    assert!(matches!(arows[0][6], Value::Structure(0x4e, _)));
    assert!(matches!(brows[0][6], Value::Structure(0x4e, _)));
    commit(&mut a);
    b.send(0x12, vec![]);
    b.failure(CONFLICT);
    assert!(world(&server).entity_by_handle("allocated_a").is_some());
    assert!(world(&server).entity_by_handle("allocated_b").is_none());
}

#[test]
fn query_read_sets_prevent_write_skew_but_allow_unrelated_attributes() {
    let server = writable_server();
    let mut a = Client::ready(&server);
    let mut b = Client::ready(&server);
    begin(&mut a);
    begin(&mut b);
    let occupants = "// kala:datalog\n[:find ?e :at -300 :where [?e :located_in :varrow]]";
    assert_eq!(a.query(occupants), b.query(occupants));
    write(&mut a, "[:set :greg :located_in :carnath :at 700]");
    write(&mut b, "[:set :tomas :located_in :ashford :at 700]");
    commit(&mut a);
    b.send(0x12, vec![]);
    b.failure(CONFLICT);
    b.reset();
    begin(&mut a);
    begin(&mut b);
    a.query("// kala:datalog\n[:find ?n :where [:greg :name ?n]]");
    b.query("// kala:datalog\n[:find ?a :where [:greg :alias ?a]]");
    write(&mut a, "[:assert :greg :disguised true :at 701]");
    write(&mut b, "[:assert :tomas :disguised true :at 701]");
    commit(&mut a);
    commit(&mut b);
}

#[test]
fn transaction_and_autocommit_option_types_and_write_parameters_are_checked() {
    let server = writable_server();
    let mut client = Client::ready(&server);
    for extra in [
        map(&[("mode", Value::Integer(1))]),
        map(&[("mode", string("invalid"))]),
        map(&[("tx_metadata", string("not a map"))]),
        map(&[(
            "tx_metadata",
            map(&[("concurrency_mode", string("invalid"))]),
        )]),
        map(&[("tx_timeout", string("soon"))]),
        map(&[("tx_timeout", Value::Integer(-1))]),
        map(&[("db", Value::Integer(1))]),
    ] {
        for tag in [0x11, 0x10] {
            let fields = if tag == 0x11 {
                vec![extra.clone()]
            } else {
                vec![string(WRITE), map(&[]), extra.clone()]
            };
            client.send(tag, fields);
            client.failure(INVALID);
            client.reset();
        }
    }
    client.send(
        0x10,
        vec![
            string(WRITE),
            map(&[("unused", Value::Integer(1))]),
            map(&[]),
        ],
    );
    let failure = client.failure(INVALID);
    assert!(failure_text(&failure).contains("param"));
}

#[test]
fn read_mode_refuses_writes_and_read_only_transactions_work() {
    let server = writable_server();
    let mut client = Client::ready(&server);
    client.send(0x11, vec![map(&[("mode", string("r"))])]);
    client.success();
    assert_eq!(client.query(client::ALLIES).len(), 2);
    client.send(0x10, vec![string(WRITE), map(&[]), map(&[])]);
    client.failure(ACCESS_MODE);
    client.reset();
    client.send(
        0x10,
        vec![string(WRITE), map(&[]), map(&[("mode", string("r"))])],
    );
    client.failure(ACCESS_MODE);
    client.reset();
    client.send(0x11, vec![map(&[("mode", string("r"))])]);
    client.success();
    assert_eq!(client.query(client::ALLIES).len(), 2);
    let summary = commit(&mut client);
    assert_eq!(summary.get("type"), Some(&string("r")));
}

fn privilege_server() -> Running {
    let dir = tempfile::tempdir().unwrap();
    drop(kala_fixture::load_fixture_to(VARROW, &dir.path().join("varrow.kala")).unwrap());
    for (name, grants) in [
        ("reader", vec!["read"]),
        ("writer", vec!["write"]),
        ("both", vec!["read", "write"]),
    ] {
        let account = format!("{name}@localhost");
        kala_system::system::add(dir.path(), &account, None).unwrap();
        for grant in grants {
            kala_system::system::grant(dir.path(), &account, grant, "varrow").unwrap();
        }
    }
    let server = Server::bind_accounts(
        dir.path(),
        ServerConfig {
            listen: vec!["127.0.0.1:0".parse().unwrap()],
            default_db: Some("varrow".into()),
            ..ServerConfig::default()
        },
    )
    .unwrap();
    Running::from_server(dir, server)
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

#[test]
fn privilege_checks_cover_auto_explicit_and_hidden_multi_form_writes() {
    let server = privilege_server();
    let mut reader = login(&server, "reader");
    for explicit in [false, true] {
        if explicit {
            begin(&mut reader);
        }
        reader.send(0x10, vec![string(WRITE), map(&[]), map(&[])]);
        let failure = reader.failure(FORBIDDEN);
        assert!(failure_text(&failure).contains("write"));
        reader.reset();
    }
    let mut writer = login(&server, "writer");
    write(&mut writer, "[:assert :greg :alias \"Blind\" :at 300]");
    for body in [
        "[:set :greg :alias \"Reading\" :at 301]",
        "[:create :CHARACTER :greg :if-missing]",
        "[:correct 1 :delete]",
        "[[:assert :greg :alias \"Hidden\" :at 301] [:set :greg :alias \"Reading\" :at 302]]",
    ] {
        for explicit in [false, true] {
            if explicit {
                begin(&mut writer);
            }
            let before = std::fs::read(server.dir.path().join("varrow.kala")).unwrap();
            writer.send(
                0x10,
                vec![
                    string(&format!("// kala:datalog\n{body}")),
                    map(&[]),
                    map(&[]),
                ],
            );
            let failure = writer.failure(FORBIDDEN);
            assert!(failure_text(&failure).contains("read"));
            writer.reset();
            assert_eq!(
                std::fs::read(server.dir.path().join("varrow.kala")).unwrap(),
                before
            );
        }
    }
    let mut both = login(&server, "both");
    write(&mut both, "[:set :greg :alias \"Authorized\" :at 302]");
    assert!(
        both.query("// kala:datalog\n[:find ?a :at 302 :where [:greg :alias ?a]]")
            .contains(&vec![string("Authorized")])
    );
    let no_auth_server = Running::start();
    let mut no_auth = Client::ready(&no_auth_server);
    no_auth.send(0x10, vec![string(WRITE), map(&[]), map(&[])]);
    no_auth.failure(FORBIDDEN);
}

#[test]
fn system_write_is_forbidden_even_for_admin_and_never_changes_bytes() {
    let server = privilege_server();
    let original = std::fs::read(server.dir.path().join("system.kala")).unwrap();
    for user in ["root", "reader", "writer", "both"] {
        let mut client = login(&server, user);
        for explicit in [false, true] {
            if explicit {
                // An admin's BEGIN on `system` pins a read snapshot (drivers
                // begin in write mode for reads too); the write form is
                // what is refused. Everyone else is refused at BEGIN.
                client.send(
                    0x11,
                    vec![map(&[("db", string("system")), ("mode", string("w"))])],
                );
                if user == "root" {
                    client.success();
                    client.send(
                        0x10,
                        vec![
                            string("// kala:datalog\n[:create :ACCOUNT :backdoor]"),
                            map(&[]),
                            map(&[]),
                        ],
                    );
                }
                client.failure(FORBIDDEN);
            } else {
                client.send(
                    0x10,
                    vec![
                        string("// kala:datalog\n[:create :ACCOUNT :backdoor]"),
                        map(&[]),
                        map(&[("db", string("system"))]),
                    ],
                );
                client.failure(FORBIDDEN);
            }
            client.reset();
            assert_eq!(
                std::fs::read(server.dir.path().join("system.kala")).unwrap(),
                original
            );
        }
    }
}

#[test]
fn write_only_identity_does_not_receive_held_values_in_errors_or_notifications() {
    let server = privilege_server();
    let mut root = Client::ready(&server);
    write(
        &mut root,
        "[:assert :greg :secret_property \"held-secret\" :at 300]",
    );
    let mut writer = login(&server, "writer");
    let (rows, summary) = write(
        &mut writer,
        "[:assert :greg :secret_property \"new-value\" :at 301]",
    );
    assert!(!format!("{rows:?}{summary:?}").contains("held-secret"));
    writer.send(
        0x10,
        vec![
            string("// kala:datalog\n[:retract :greg :secret_property \"held-secret\" :at 300]"),
            map(&[]),
            map(&[]),
        ],
    );
    let failure = client::metadata(writer.recv(), 0x7f);
    assert!(!failure_text(&failure).contains("held-secret"));
    assert!(failure_text(&failure).contains("read"));
}

#[test]
fn bookmarks_are_checked_and_causal_read_sees_commit() {
    let server = writable_server();
    let mut writer = Client::ready(&server);
    let (_, summary) = write(&mut writer, "[:assert :greg :alias \"Causal\" :at 300]");
    let bookmark = summary["bookmark"].clone();
    let mut reader = Client::ready(&server);
    reader.run(
        ALIASES,
        map(&[]),
        map(&[("bookmarks", Value::List(vec![bookmark.clone()]))]),
    );
    assert!(reader.pull(-1).0.contains(&vec![string("Causal")]));
    reader.send(
        0x11,
        vec![map(&[
            ("mode", string("r")),
            ("bookmarks", Value::List(vec![bookmark])),
        ])],
    );
    reader.success();
    assert!(reader.query(ALIASES).contains(&vec![string("Causal")]));
    commit(&mut reader);
    for bookmarks in [
        Value::List(vec![string("invalid")]),
        Value::List(vec![string("kala:varrow:-1")]),
        Value::List(vec![Value::Integer(1)]),
        string("kala:varrow:1"),
    ] {
        for tag in [0x11, 0x10] {
            let extra = map(&[("bookmarks", bookmarks.clone())]);
            reader.send(
                tag,
                if tag == 0x11 {
                    vec![extra]
                } else {
                    vec![string(ALIASES), map(&[]), extra]
                },
            );
            reader.failure("Neo.ClientError.Transaction.InvalidBookmark");
            reader.reset();
        }
    }
}

#[test]
fn inferred_schema_overlap_and_latest_tick_movement_are_notified() {
    let server = writable_server();
    let mut a = Client::ready(&server);
    let mut b = Client::ready(&server);
    let (_, declared) = write(&mut a, "[:assert :greg :bolt_inferred 19 :at 300]");
    let text = format!("{:?}", declared.get("notifications"));
    assert!(text.contains("inferred"), "{text}");
    assert!(text.contains("bolt_inferred"), "{text}");
    let (_, overlap) = write(&mut a, "[:assert :greg :bolt_inferred 20 :at 301]");
    let text = format!("{:?}", overlap.get("notifications"));
    assert!(text.contains("Kala.Warning.Overlap"), "{text}");
    // The latest tick is the one at BEGIN: a commit that lands between
    // BEGIN and the transaction's first write (when its writable handle
    // opens) doesn't move it, and the commit says that it moved.
    let pinned_tick = world(&server).latest_tick().0;
    begin(&mut a);
    write(&mut b, "[:assert :greg :alias \"Later\" :at 999]");
    let (rows, _) = write(&mut a, "[:assert :greg :alias \"Pinned\"]");
    assert_eq!(rows[0][3], Value::Integer(pinned_tick));
    let moved = commit(&mut a);
    let text = format!("{moved:?}");
    assert!(text.contains("latest_tick_moved"), "{text}");
    assert!(
        text.contains(&format!("latest tick moved {pinned_tick} → 999")),
        "{text}"
    );
    let greg = world(&server).entity_by_handle("greg").unwrap();
    let history = world(&server).history(greg, "alias").unwrap();
    let pinned = history
        .iter()
        .find(|change| change.value == kala_core::Value::Str("Pinned".into()))
        .unwrap();
    assert_eq!(pinned.tick.0, pinned_tick);
    // The same when the other commit lands after the first write.
    let pinned_tick = world(&server).latest_tick().0;
    assert_eq!(pinned_tick, 999);
    begin(&mut a);
    let (rows, _) = write(&mut a, "[:assert :greg :alias \"Pinned again\"]");
    assert_eq!(rows[0][3], Value::Integer(pinned_tick));
    write(&mut b, "[:assert :greg :alias \"Later still\" :at 1999]");
    let (rows, _) = write(&mut a, "[:assert :greg :alias \"Pinned too\"]");
    assert_eq!(rows[0][3], Value::Integer(pinned_tick));
    let moved = commit(&mut a);
    let text = format!("{moved:?}");
    assert!(text.contains("latest_tick_moved"), "{text}");
    assert!(text.contains("latest tick moved 999 → 1999"), "{text}");
    // No movement, no notification.
    begin(&mut a);
    write(&mut a, "[:assert :greg :alias \"Unmoved\"]");
    let text = format!("{:?}", commit(&mut a));
    assert!(!text.contains("latest_tick_moved"), "{text}");
}

#[test]
fn write_ticks_cover_negative_zero_and_extremes_with_retraction() {
    let server = writable_server();
    let mut client = Client::ready(&server);
    for tick in [i64::MIN, -1, 0, i64::MAX] {
        let value = format!("tick-{tick}");
        let (rows, _) = write(
            &mut client,
            &format!("[:assert :greg :alias \"{value}\" :at {tick}]"),
        );
        assert_eq!(rows[0][3], Value::Integer(tick));
        assert!(
            values(&server, "greg", "alias", tick).contains(&kala_core::Value::Str(value.clone()))
        );
        if tick < i64::MAX {
            write(
                &mut client,
                &format!("[:retract :greg :alias \"{value}\" :at {}]", tick + 1),
            );
            assert!(
                !values(&server, "greg", "alias", tick + 1).contains(&kala_core::Value::Str(value))
            );
        }
    }
}

#[cfg(unix)]
#[test]
fn unix_transaction_lifecycle_and_privileges_match_tcp() {
    use std::os::unix::net::UnixStream;
    use std::time::Duration;
    let dir = tempfile::tempdir().unwrap();
    drop(kala_fixture::load_fixture_to(VARROW, &dir.path().join("varrow.kala")).unwrap());
    let socket = dir.path().join("bolt.sock");
    let server = Server::bind_accounts(
        dir.path(),
        ServerConfig {
            listen: vec![
                "127.0.0.1:0".parse().unwrap(),
                kala_bolt::transport::ListenAddr::Unix(socket.clone()),
            ],
            default_db: Some("varrow".into()),
            ..ServerConfig::default()
        },
    )
    .unwrap();
    let server = Running::from_server(dir, server);
    let connect = || {
        let mut stream = UnixStream::connect(&socket).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        stream
            .set_write_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        common::handshake(&mut stream, 4);
        let mut client = Client { stream };
        client.hello();
        client.success();
        client.send(0x6a, vec![map(&[("scheme", string("none"))])]);
        client.success();
        client
    };
    let mut client = connect();
    let before = world(&server).latest_tx().0;
    begin(&mut client);
    write(
        &mut client,
        "[:assert :greg :alias \"Unix committed\" :at 300]",
    );
    commit(&mut client);
    assert_eq!(world(&server).latest_tx().0, before + 1);
    for ending in [0x13, 0x0f] {
        begin(&mut client);
        write(&mut client, "[:create :CHARACTER :unix_discarded]");
        client.send(ending, vec![]);
        client.success();
        assert!(world(&server).entity_by_handle("unix_discarded").is_none());
    }
    begin(&mut client);
    client.send(0x11, vec![map(&[])]);
    client.failure(INVALID);
    client.send(0x12, vec![]);
    assert!(client::structure(client.recv(), 0x7e).is_empty());
    client.reset();
    begin(&mut client);
    client.run_empty(client::NAMES);
    client.send(0x12, vec![]);
    client.failure(INVALID);
    client.reset();
    for tag in [0x12, 0x13] {
        client.send(tag, vec![]);
        client.failure(INVALID);
        client.reset();
    }
    // BEGIN on `system` is a read snapshot for an admin; a write form in it
    // is refused.
    client.send(0x11, vec![map(&[("db", string("system"))])]);
    client.success();
    client.send(
        0x10,
        vec![
            string("// kala:datalog\n[:create :ACCOUNT :backdoor]"),
            map(&[]),
            map(&[]),
        ],
    );
    client.failure(FORBIDDEN);
    client.reset();
    begin(&mut client);
    write(&mut client, "[:create :CHARACTER :unix_dropped]");
    drop(client);
    assert!(world(&server).entity_by_handle("unix_dropped").is_none());
    let mut next = connect();
    assert_eq!(next.query(client::ALLIES).len(), 2);
}

#[test]
fn schema_only_and_read_write_summaries_have_distinct_types() {
    let server = writable_server();
    let mut client = Client::ready(&server);
    let (_, schema) = write(&mut client, "[:attribute :bolt_declared :i64]");
    assert_eq!(schema["type"], string("s"));
    begin(&mut client);
    assert_eq!(client.query(client::ALLIES).len(), 2);
    write(&mut client, "[:assert :greg :alias \"Mixed\" :at 300]");
    let summary = commit(&mut client);
    assert_eq!(summary["type"], string("rw"));
}

#[test]
fn rollback_after_malformed_write_does_not_escape_failed_state() {
    let server = writable_server();
    let mut client = Client::ready(&server);
    let before = world(&server).latest_tx();
    begin(&mut client);
    write(&mut client, "[:assert :greg :alias \"Pending\" :at 300]");
    client.send(
        0x10,
        vec![string("// kala:datalog\n[:assert"), map(&[]), map(&[])],
    );
    client.failure("Neo.ClientError.Statement.SyntaxError");
    for (tag, fields) in [
        (0x12, vec![]),
        (0x13, vec![]),
        (0x11, vec![map(&[])]),
        (0x10, vec![string(WRITE), map(&[]), map(&[])]),
    ] {
        client.send(tag, fields);
        assert!(client::structure(client.recv(), 0x7e).is_empty());
    }
    client.reset();
    assert_eq!(world(&server).latest_tx(), before);
    write(
        &mut client,
        "[:assert :greg :alias \"After reset\" :at 300]",
    );
}

#[test]
fn write_only_create_reports_only_own_node_identity() {
    let server = privilege_server();
    let mut client = login(&server, "writer");
    let (rows, _) = write(
        &mut client,
        "[[:create :CHARACTER :created_blind] [:assert :created_blind :alias \"Own text\" :at 300]]",
    );
    assert_eq!(rows[0][7], Value::Bool(true));
    let Value::Structure(0x4e, fields) = &rows[0][6] else {
        panic!("newly-created node should retain its ID")
    };
    assert_eq!(
        fields[2],
        map(&[]),
        "write-only node must not carry materialized properties"
    );
    assert_eq!(rows[1][6], Value::Null);
    assert_eq!(rows[1][7], Value::Null);
}

#[test]
fn store_rule_failures_are_statement_errors_and_leave_the_log_unchanged() {
    let server = writable_server();
    let mut client = Client::ready(&server);
    write(
        &mut client,
        "[:assert :greg :alias \"Already here\" :at 300]",
    );
    write(
        &mut client,
        "[[:attribute :retired :string] [:deprecate :retired]]",
    );
    for (form, detail, expected_code) in [
        (
            "[:assert :greg :alias 7 :at 301]",
            "values",
            "Neo.ClientError.Statement.TypeError",
        ),
        (
            "[:create :CHARACTER :greg]",
            "already",
            "Neo.ClientError.Schema.ConstraintValidationFailed",
        ),
        (
            "[:assert :absent_handle :alias \"x\" :at 301]",
            "handle",
            "Neo.ClientError.Statement.SemanticError",
        ),
        (
            "[:assert :greg :retired \"x\" :at 301]",
            "deprecated",
            "Neo.ClientError.Statement.SemanticError",
        ),
        (
            "[:retract :greg :born_in :varrow :at 301]",
            "event",
            "Neo.ClientError.Statement.SemanticError",
        ),
        (
            "[:retract :greg :alias \"Already here\" :at 300]",
            "same-tick conflict",
            "Neo.ClientError.Schema.ConstraintValidationFailed",
        ),
    ] {
        let before = std::fs::read(server.dir.path().join("varrow.kala")).unwrap();
        client.send(
            0x10,
            vec![
                string(&format!("// kala:datalog\n{form}")),
                map(&[]),
                map(&[]),
            ],
        );
        let failure = client::metadata(client.recv(), 0x7f);
        assert_eq!(failure.get("code"), Some(&string(expected_code)));
        assert!(
            failure_text(&failure).to_lowercase().contains(detail),
            "{failure:?}"
        );
        assert_eq!(
            std::fs::read(server.dir.path().join("varrow.kala")).unwrap(),
            before
        );
        client.reset();
    }
}

#[test]
fn damaged_tail_requires_operator_acknowledgement_and_server_never_cuts_it() {
    use std::io::Write;
    for warm_writer in [false, true] {
        let server = writable_server();
        let mut client = Client::ready(&server);
        if warm_writer {
            write(
                &mut client,
                "[:assert :greg :alias \"Warm writer\" :at 300]",
            );
        }
        let path = server.dir.path().join("varrow.kala");
        let before = std::fs::read(&path).unwrap();
        let scratch = tempfile::tempdir().unwrap();
        let copy = scratch.path().join("copy.kala");
        std::fs::write(&copy, &before).unwrap();
        let mut w = World::open(&copy).unwrap();
        let greg = w.entity_by_handle("greg").unwrap();
        w.assert(
            greg,
            "alias",
            kala_core::Value::Str("Lost write".into()),
            Some(kala_core::Tick(301)),
        )
        .unwrap();
        drop(w);
        let mut damaged = std::fs::read(&copy).unwrap()[before.len()..].to_vec();
        damaged[30] ^= 1;
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        file.write_all(&damaged).unwrap();
        file.sync_all().unwrap();
        let original = std::fs::read(&path).unwrap();
        for _ in 0..2 {
            client.send(0x10, vec![string(WRITE), map(&[]), map(&[])]);
            let failure = client::metadata(client.recv(), 0x7f);
            assert!(
                matches!(failure.get("code"), Some(Value::String(code)) if code.starts_with("Neo.DatabaseError.")),
                "{failure:?}"
            );
            assert!(
                failure_text(&failure).contains("--acknowledge-recovery"),
                "{failure:?}"
            );
            assert_eq!(
                std::fs::read(&path).unwrap(),
                original,
                "server must never acknowledge or cut damaged bytes"
            );
            assert!(std::fs::read_dir(server.dir.path()).unwrap().all(|entry| {
                !entry
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .contains(".torn-")
            }));
            client.reset();
        }
    }
}

#[test]
fn diverged_cached_writer_is_reopened_and_retry_succeeds() {
    let server = writable_server();
    let path = server.dir.path().join("varrow.kala");
    let original = std::fs::read(&path).unwrap();
    let mut client = Client::ready(&server);
    write(
        &mut client,
        "[:assert :greg :alias \"Removed tip\" :at 300]",
    );
    // Simulate an independently recovered/replaced log without touching a
    // committed prefix. The cached writer must reject its now-stale history.
    std::fs::write(&path, &original).unwrap();
    client.send(0x10, vec![string(WRITE), map(&[]), map(&[])]);
    let failure = client::metadata(client.recv(), 0x7f);
    assert!(
        matches!(failure.get("code"), Some(Value::String(code)) if code.starts_with("Neo.TransientError.Transaction.")),
        "{failure:?}"
    );
    assert_eq!(std::fs::read(&path).unwrap(), original);
    client.reset();
    write(
        &mut client,
        "[:assert :greg :alias \"Retry succeeded\" :at 300]",
    );
    let held = values(&server, "greg", "alias", 300);
    assert!(!held.contains(&kala_core::Value::Str("Removed tip".into())));
    assert!(held.contains(&kala_core::Value::Str("Retry succeeded".into())));
}

/// Begins a transaction and makes it write, retrying until a write slot is
/// free: the slot is taken at a transaction's first write form, not at BEGIN.
fn await_write_begin(client: &mut Client) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        begin(client);
        client.send(
            0x10,
            vec![
                string("// kala:datalog\n[:create :CHARACTER :slot_probe]"),
                map(&[]),
                map(&[]),
            ],
        );
        let response = client.recv();
        if matches!(response, Value::Structure(0x70, _)) {
            client.pull(-1);
            return;
        }
        let failure = client::metadata(response, 0x7f);
        assert!(
            matches!(failure.get("code"), Some(Value::String(code)) if code.starts_with("Neo.TransientError.Transaction.")),
            "{failure:?}"
        );
        assert!(
            std::time::Instant::now() < deadline,
            "write transaction slot was not reclaimed"
        );
        client.reset();
    }
}

#[test]
fn write_transaction_cap_releases_slots_on_every_ending() {
    let server = writable_server_with(ServerConfig {
        max_write_transactions: 1,
        ..Default::default()
    });
    for (index, ending) in [0x12, 0x13, 0x0f, 0x02, 0x00].into_iter().enumerate() {
        let mut a = Client::ready(&server);
        let mut b = Client::ready(&server);
        begin(&mut a);
        write(&mut a, &format!("[:create :CHARACTER :cap_{index}]"));
        // The slot is taken by a transaction's first write form.
        begin(&mut b);
        b.send(
            0x10,
            vec![
                string("// kala:datalog\n[:create :CHARACTER :over_the_cap]"),
                map(&[]),
                map(&[]),
            ],
        );
        let failure = client::metadata(b.recv(), 0x7f);
        assert!(
            matches!(failure.get("code"), Some(Value::String(code)) if code.starts_with("Neo.TransientError.Transaction.")),
            "{failure:?}"
        );
        assert!(failure_text(&failure).contains("too many open write transactions"));
        b.reset();
        if ending == 0x00 {
            drop(a);
        } else {
            a.send(ending, vec![]);
            if ending == 0x02 {
                common::assert_closed(&mut a.stream);
            } else {
                a.success();
            }
        }
        await_write_begin(&mut b);
        b.send(0x13, vec![]);
        b.success();
        assert_eq!(
            world(&server)
                .entity_by_handle(&format!("cap_{index}"))
                .is_some(),
            ending == 0x12
        );
    }
}

#[test]
fn total_idle_and_client_timeouts_rollback_and_release_slots_while_idle() {
    use std::time::Duration;
    for timeout_kind in 0..3 {
        let server = writable_server_with(ServerConfig {
            max_write_transactions: 1,
            tx_timeout: if timeout_kind == 0 {
                Duration::from_millis(500)
            } else {
                Duration::from_secs(5)
            },
            tx_idle_timeout: if timeout_kind == 1 {
                Duration::from_millis(500)
            } else {
                Duration::from_secs(5)
            },
            ..Default::default()
        });
        let mut a = Client::ready(&server);
        let mut b = Client::ready(&server);
        a.send(
            0x11,
            vec![if timeout_kind == 2 {
                map(&[("tx_timeout", Value::Integer(500))])
            } else {
                map(&[])
            }],
        );
        a.success();
        write(&mut a, "[:create :CHARACTER :expired]");
        // A stays completely idle. B's eventual successful BEGIN proves the
        // deadline released the slot without relying on another message on A.
        await_write_begin(&mut b);
        b.send(0x13, vec![]);
        b.success();
        a.send(0x12, vec![]);
        a.failure("Neo.ClientError.Transaction.TransactionTimedOut");
        a.reset();
        assert!(world(&server).entity_by_handle("expired").is_none());
        assert_eq!(a.query(client::ALLIES).len(), 2);
    }
}

#[test]
fn read_only_server_refuses_write_begin_and_autocommit_before_staging() {
    let server = writable_server_with(ServerConfig {
        read_only: true,
        ..Default::default()
    });
    let before = std::fs::read(server.dir.path().join("varrow.kala")).unwrap();
    let mut client = Client::ready(&server);
    // A write-mode BEGIN is only a read snapshot until a write form
    // arrives (drivers begin in write mode for reads too), so the refusal
    // is at the write form, inside a transaction as outside one.
    for explicit in [true, false] {
        if explicit {
            client.send(0x11, vec![map(&[("mode", string("w"))])]);
            client.success();
        }
        client.send(0x10, vec![string(WRITE), map(&[]), map(&[])]);
        let failure = client.failure(FORBIDDEN);
        assert!(failure_text(&failure).contains("this server is read-only"));
        client.reset();
        assert_eq!(
            std::fs::read(server.dir.path().join("varrow.kala")).unwrap(),
            before
        );
    }
    client.send(0x11, vec![map(&[("mode", string("r"))])]);
    client.success();
    assert_eq!(client.query(client::ALLIES).len(), 2);
    commit(&mut client);
}

#[test]
fn explicit_transaction_form_cap_is_cumulative_across_runs() {
    let server = writable_server();
    let before = world(&server).latest_tx();
    let mut client = Client::ready(&server);
    begin(&mut client);
    let body = (0..256)
        .map(|n| format!("[:assert :greg :alias \"bounded-{n}\" :at 300]"))
        .collect::<Vec<_>>()
        .join("\n");
    let (rows, _) = write(&mut client, &body);
    assert_eq!(rows.len(), 256);
    client.send(0x10, vec![string(WRITE), map(&[]), map(&[])]);
    let failure = client::metadata(client.recv(), 0x7f);
    assert!(failure_text(&failure).contains("256"), "{failure:?}");
    client.reset();
    assert_eq!(world(&server).latest_tx(), before);
    begin(&mut client);
    client.send(
        0x10,
        vec![
            string(&format!(
                "// kala:datalog\n{body}\n[:assert :greg :alias \"257\" :at 300]"
            )),
            map(&[]),
            map(&[]),
        ],
    );
    let failure = client::metadata(client.recv(), 0x7f);
    assert!(failure_text(&failure).contains("256"), "{failure:?}");
    client.reset();
    assert_eq!(world(&server).latest_tx(), before);
}

#[test]
fn metadata_bookmark_count_and_lengths_are_bounded() {
    let server = writable_server();
    let mut client = Client::ready(&server);
    for extra in [
        map(&[(
            "tx_metadata",
            map(&[("oversize", string(&"x".repeat(65_537)))]),
        )]),
        map(&[(
            "bookmarks",
            Value::List(vec![string("kala:varrow:1"); 1025]),
        )]),
        map(&[(
            "bookmarks",
            Value::List(vec![string(&format!("kala:varrow:{}", "1".repeat(2048)))]),
        )]),
    ] {
        client.send(0x11, vec![extra]);
        let failure = client::metadata(client.recv(), 0x7f);
        assert!(
            matches!(failure.get("code"), Some(Value::String(code)) if code == INVALID || code == "Neo.ClientError.Transaction.InvalidBookmark"),
            "{failure:?}"
        );
        client.reset();
    }
}

#[test]
fn future_bookmark_wait_is_bounded_and_transient() {
    let server = writable_server_with(ServerConfig {
        busy_timeout: std::time::Duration::from_millis(80),
        ..Default::default()
    });
    let mut client = Client::ready(&server);
    for tag in [0x11, 0x10] {
        let extra = map(&[(
            "bookmarks",
            Value::List(vec![string("kala:varrow:999999999")]),
        )]);
        let start = std::time::Instant::now();
        client.send(
            tag,
            if tag == 0x11 {
                vec![extra]
            } else {
                vec![string(ALIASES), map(&[]), extra]
            },
        );
        let failure = client::metadata(client.recv(), 0x7f);
        assert!(
            matches!(failure.get("code"), Some(Value::String(code)) if code.starts_with("Neo.TransientError.Transaction.")),
            "{failure:?}"
        );
        assert!(start.elapsed() < std::time::Duration::from_secs(2));
        client.reset();
    }
}

#[test]
fn held_file_lock_returns_transient_failure_and_retry_succeeds() {
    let server = writable_server_with(ServerConfig {
        busy_timeout: std::time::Duration::from_millis(80),
        ..Default::default()
    });
    let mut client = Client::ready(&server);
    write(
        &mut client,
        "[:assert :greg :alias \"Before lock\" :at 300]",
    );
    let path = server.dir.path().join("varrow.kala");
    let before = std::fs::read(&path).unwrap();
    let mut holder = World::open(&path).unwrap();
    holder.with_writer_lock(|| {
        let start = std::time::Instant::now();
        client.send(0x10, vec![string(WRITE), map(&[]), map(&[])]);
        let failure = client.failure("Neo.TransientError.Transaction.LockAcquisitionTimeout");
        assert!(failure_text(&failure).contains("another writer held the world's lock for the whole busy timeout; nothing was written"), "{failure:?}");
        assert!(start.elapsed() < std::time::Duration::from_secs(2));
        assert_eq!(std::fs::read(&path).unwrap(), before);
    }).unwrap();
    client.reset();
    write(&mut client, "[:assert :greg :alias \"After lock\" :at 300]");
}

#[test]
fn overlap_notifications_cap_values_and_report_exact_omitted_count() {
    let server = writable_server();
    let mut client = Client::ready(&server);
    let operations = (0..16)
        .map(|n| format!("[:assert :greg :capped_overlap {n} :at 800]"))
        .collect::<Vec<_>>()
        .join(" ");
    let (_, summary) = write(&mut client, &format!("[{operations}]"));
    let Value::List(notifications) = &summary["notifications"] else {
        panic!("expected notifications list")
    };
    let overlap_count = notifications.iter().filter(|item| matches!(item, Value::Map(map) if map.get("code") == Some(&string("Kala.Warning.Overlap")))).count();
    assert_eq!(overlap_count, 100);
    let omitted = notifications.iter().find(|item| matches!(item, Value::Map(map) if map.get("code") == Some(&string("Kala.Information.OverlapsOmitted")))).unwrap();
    assert!(format!("{omitted:?}").contains("20 introduced"));
}

#[test]
fn cumulative_staged_text_has_a_byte_cap_and_failed_run_rolls_back() {
    let server = writable_server();
    let mut client = Client::ready(&server);
    let before = world(&server).latest_tx();
    begin(&mut client);
    // A RUN is separately bounded to 64 KiB by Datalog. Pipeline near-limit
    // forms to exercise the transaction bound without 256 network round trips.
    let text = format!(
        "// kala:datalog\n[:assert :greg :biography \"{}\" :at 800]",
        "x".repeat(65_000)
    );
    for _ in 0..256 {
        client.send(0x10, vec![string(&text), map(&[]), map(&[])]);
        client.send(0x3f, vec![map(&[("n", Value::Integer(-1))])]);
    }
    for _ in 0..256 {
        client.success();
        client.records();
    }
    client.send(0x10, vec![string(&text), map(&[]), map(&[])]);
    let failure = client.failure(INVALID);
    assert!(
        failure_text(&failure).contains("staged-text byte limit")
            || failure_text(&failure).contains("256-form limit"),
        "{failure:?}"
    );
    client.reset();
    assert_eq!(world(&server).latest_tx(), before);
}

#[test]
fn slow_reader_cannot_keep_explicit_write_slot_past_transaction_timeout() {
    use std::time::{Duration, Instant};
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
    let server = Server::bind_accounts(
        dir.path(),
        ServerConfig {
            listen: vec!["127.0.0.1:0".parse().unwrap()],
            default_db: Some("varrow".into()),
            max_write_transactions: 1,
            tx_timeout: Duration::from_millis(1200),
            tx_idle_timeout: Duration::from_secs(10),
            write_timeout: Duration::from_secs(10),
            ..Default::default()
        },
    )
    .unwrap();
    let server = Running::from_server(dir, server);
    let mut slow = Client::ready(&server);
    let mut other = Client::ready(&server);
    begin(&mut slow);
    write(&mut slow, "[:create :CHARACTER :slow_reader_staged]");
    slow.run_empty("// kala:datalog\n[:find ?n ?b :where [?e :name ?n] [:greg :biography ?b]]");
    slow.send(0x3f, vec![map(&[("n", Value::Integer(-1))])]);
    // More than 10 MiB is waiting for a client that consumes no result bytes.
    // Reclaiming the transaction's slot proves client I/O obeys its deadline.
    let start = Instant::now();
    await_write_begin(&mut other);
    assert!(start.elapsed() < Duration::from_secs(4));
    other.send(0x13, vec![]);
    other.success();
    assert!(
        world(&server)
            .entity_by_handle("slow_reader_staged")
            .is_none()
    );
}

#[test]
fn cached_auto_writer_does_not_commit_after_client_timeout_while_waiting_for_lock() {
    use std::time::{Duration, Instant};
    let server = writable_server_with(ServerConfig {
        busy_timeout: Duration::from_secs(3),
        ..Default::default()
    });
    let mut client = Client::ready(&server);
    let mut clock_client = Client::ready(&server);
    write(
        &mut client,
        "[:assert :greg :alias \"Warm cached handle\" :at 300]",
    );
    let path = server.dir.path().join("varrow.kala");
    let before = std::fs::read(&path).unwrap();
    let mut holder = World::open(&path).unwrap();
    holder
        .with_writer_lock(|| {
            client.send(
                0x10,
                vec![
                    string(WRITE),
                    map(&[]),
                    map(&[("tx_timeout", Value::Integer(100))]),
                ],
            );
            let release_at = Instant::now() + Duration::from_millis(350);
            while Instant::now() < release_at {
                clock_client.send(0x54, vec![Value::Integer(2)]);
                clock_client.success();
            }
        })
        .unwrap();
    let failure = client::metadata(client.recv(), 0x7f);
    assert!(
        matches!(failure.get("code"), Some(Value::String(code)) if code == "Neo.ClientError.Transaction.TransactionTimedOut" || code == "Neo.TransientError.Transaction.LockAcquisitionTimeout"),
        "{failure:?}"
    );
    assert_eq!(
        std::fs::read(&path).unwrap(),
        before,
        "timeout while waiting for lock must leave no durable write"
    );
}

#[test]
fn explicit_commit_does_not_commit_after_transaction_timeout_while_waiting_for_lock() {
    use std::time::{Duration, Instant};
    let server = writable_server_with(ServerConfig {
        busy_timeout: Duration::from_secs(3),
        ..Default::default()
    });
    let mut client = Client::ready(&server);
    let mut clock_client = Client::ready(&server);
    client.send(0x11, vec![map(&[("tx_timeout", Value::Integer(500))])]);
    client.success();
    write(
        &mut client,
        "[:assert :greg :alias \"Expired lock wait\" :at 300]",
    );
    let path = server.dir.path().join("varrow.kala");
    let before = std::fs::read(&path).unwrap();
    let mut holder = World::open(&path).unwrap();
    holder
        .with_writer_lock(|| {
            client.send(0x12, vec![]);
            let release_at = Instant::now() + Duration::from_millis(700);
            while Instant::now() < release_at {
                clock_client.send(0x54, vec![Value::Integer(2)]);
                clock_client.success();
            }
        })
        .unwrap();
    client.failure("Neo.ClientError.Transaction.TransactionTimedOut");
    assert_eq!(
        std::fs::read(&path).unwrap(),
        before,
        "expired explicit COMMIT must leave the file unchanged"
    );
    client.reset();
    assert_eq!(client.query(client::ALLIES).len(), 2);
}

#[test]
fn scalar_label_queries_conflict_if_an_absent_label_becomes_a_node_type() {
    for query in [
        "MATCH (n:CHARACTER:NEWTYPE) RETURN n.name",
        "MATCH (n:NEWTYPE) RETURN n.name",
    ] {
        let server = writable_server();
        let mut a = Client::ready(&server);
        let mut b = Client::ready(&server);
        write(&mut b, "[:assert :greg :label \"NEWTYPE\" :at 800]");
        begin(&mut a);
        assert!(
            !a.query(query).is_empty(),
            "the undeclared type name initially denotes a label attribute"
        );
        write(&mut b, "[:node-type :NEWTYPE]");
        write(&mut a, "[:assert :greg :disguised true :at 801]");
        let before = std::fs::read(server.dir.path().join("varrow.kala")).unwrap();
        a.send(0x12, vec![]);
        a.failure(CONFLICT);
        assert_eq!(
            std::fs::read(server.dir.path().join("varrow.kala")).unwrap(),
            before
        );
    }
}

#[test]
fn committed_writes_with_oversized_summaries_still_report_success_and_omissions() {
    for explicit in [false, true] {
        let server = writable_server_with(ServerConfig {
            max_message_bytes: 2048,
            ..Default::default()
        });
        let mut client = Client::ready(&server);
        let before = world(&server).latest_tx().0;
        if explicit {
            begin(&mut client);
        }
        let body = (0..16)
            .map(|n| format!("[:assert :greg :bounded_notification {n} :at 800]"))
            .collect::<Vec<_>>()
            .join(" ");
        let (rows, pull_summary) = write(&mut client, &format!("[{body}]"));
        assert_eq!(rows.len(), 16);
        let summary = if explicit {
            commit(&mut client)
        } else {
            pull_summary
        };
        assert_eq!(
            summary["bookmark"],
            string(&format!("kala:varrow:{}", before + 1))
        );
        let Value::Map(kala) = &summary["kala"] else {
            panic!("commit summary needs its Kala metadata")
        };
        assert_eq!(kala["changed"], Value::Integer(16));
        assert_eq!(kala["tx"], Value::Integer((before + 1) as i64));
        assert!(
            format!("{summary:?}").contains("omitted"),
            "bounded summary must disclose omitted detail: {summary:?}"
        );
        assert_eq!(world(&server).latest_tx().0, before + 1);
        assert_eq!(
            values(&server, "greg", "bounded_notification", 800).len(),
            16
        );
    }
}

#[test]
fn discarded_provisional_ids_do_not_prevent_commit_renumbering() {
    for consume_non_entity_row in [false, true] {
        let server = writable_server();
        let mut a = Client::ready(&server);
        let mut b = Client::ready(&server);
        begin(&mut a);
        begin(&mut b);
        write(&mut a, "[:create :CHARACTER :allocated_first]");
        b.run_empty("// kala:datalog\n[[:assert :greg :alias \"No ID here\" :at 800] [:create :CHARACTER :allocated_second]]");
        if consume_non_entity_row {
            let (rows, summary) = b.pull(1);
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0][6], Value::Null);
            assert_eq!(summary.get("has_more"), Some(&Value::Bool(true)));
        }
        b.send(0x2f, vec![map(&[("n", Value::Integer(-1))])]);
        b.success();
        commit(&mut a);
        let summary = commit(&mut b);
        let Value::Map(kala) = &summary["kala"] else {
            panic!("expected Kala commit metadata")
        };
        assert!(
            matches!(kala.get("ids"), Some(Value::List(ids)) if !ids.is_empty()),
            "unexposed provisional IDs should be remapped: {kala:?}"
        );
        let w = world(&server);
        assert_ne!(
            w.entity_by_handle("allocated_first"),
            w.entity_by_handle("allocated_second")
        );
    }
}

#[test]
fn weak_find_or_create_commit_stats_count_final_creates_only() {
    let server = writable_server();
    let mut a = Client::ready(&server);
    let mut b = Client::ready(&server);
    a.send(
        0x11,
        vec![map(&[(
            "tx_metadata",
            map(&[("concurrency_mode", string("weak"))]),
        )])],
    );
    a.success();
    a.run_empty("// kala:datalog\n[:create :CHARACTER :racing_handle :if-missing]");
    a.send(0x2f, vec![map(&[("n", Value::Integer(-1))])]);
    a.success();
    write(&mut b, "[:create :CHARACTER :racing_handle]");
    let summary = commit(&mut a);
    let Value::Map(stats) = &summary["stats"] else {
        panic!("missing counters")
    };
    assert_eq!(
        stats.get("nodes-created"),
        Some(&Value::Integer(0)),
        "another transaction created the node; this commit did not"
    );
    assert_eq!(stats.get("contains-updates"), Some(&Value::Bool(false)));
}

#[test]
fn unrepresentable_transaction_duration_limits_are_rejected_without_panics() {
    let dir = tempfile::tempdir().unwrap();
    for field in ["tx_timeout", "tx_idle_timeout", "busy_timeout"] {
        let mut config = ServerConfig {
            listen: vec!["127.0.0.1:0".parse().unwrap()],
            ..Default::default()
        };
        match field {
            "tx_timeout" => config.tx_timeout = std::time::Duration::MAX,
            "tx_idle_timeout" => config.tx_idle_timeout = std::time::Duration::MAX,
            "busy_timeout" => config.busy_timeout = std::time::Duration::MAX,
            _ => unreachable!(),
        }
        let result = std::panic::catch_unwind(|| Server::bind(dir.path(), config));
        assert!(
            result.is_ok(),
            "{field} panicked instead of returning an invalid configuration error"
        );
        assert!(
            result.unwrap().is_err(),
            "{field} cannot be represented as a transaction deadline"
        );
    }
}

#[test]
fn client_query_timeout_interrupts_active_execution_and_session_recovers() {
    let server = writable_server();
    let mut client = Client::ready(&server);
    assert_eq!(client.query(client::ALLIES).len(), 2);
    client.send(
        0x10,
        vec![
            string(
                "MATCH (a:CHARACTER), (b:CHARACTER), (c:CHARACTER), (d:CHARACTER) RETURN count(*)",
            ),
            map(&[]),
            map(&[("tx_timeout", Value::Integer(1))]),
        ],
    );
    client.failure("Neo.ClientError.Transaction.TransactionTimedOut");
    client.reset();
    assert_eq!(client.query(client::ALLIES).len(), 2);
}

#[test]
fn server_statements_and_writes_cannot_share_an_explicit_transaction_in_either_order() {
    let server = writable_server();
    let mut client = Client::ready(&server);
    for statement in ["SHOW DATABASES", "CALL dbms.components()"] {
        for metadata_first in [true, false] {
            let before = std::fs::read(server.dir.path().join("varrow.kala")).unwrap();
            begin(&mut client);
            if metadata_first {
                assert!(!client.query(statement).is_empty());
                client.send(0x10, vec![string(WRITE), map(&[]), map(&[])]);
            } else {
                write(
                    &mut client,
                    "[:assert :greg :alias \"Metadata rollback\" :at 800]",
                );
                client.send(0x10, vec![string(statement), map(&[]), map(&[])]);
            }
            let failure = client.failure(INVALID);
            assert!(
                failure_text(&failure).contains("server statement"),
                "{failure:?}"
            );
            client.reset();
            assert_eq!(
                std::fs::read(server.dir.path().join("varrow.kala")).unwrap(),
                before
            );
        }
    }
}

#[test]
fn client_timeout_also_covers_metadata_pipeline_execution() {
    let server = writable_server();
    let mut client = Client::ready(&server);
    // Heavy, and inside the 16 MiB budget a list parameter is charged
    // against when it is bound (one `Datum` per element).
    let payload = Value::List(vec![Value::Integer(1); 200_000]);
    client.send(
        0x10,
        vec![
            string("CALL dbms.components() YIELD name WITH $payload AS payload RETURN payload"),
            map(&[("payload", payload)]),
            map(&[("tx_timeout", Value::Integer(1))]),
        ],
    );
    client.failure("Neo.ClientError.Transaction.TransactionTimedOut");
    client.reset();
    assert_eq!(
        client.query("CALL dbms.components() YIELD name RETURN name"),
        vec![vec![string("Kala")]]
    );
}
