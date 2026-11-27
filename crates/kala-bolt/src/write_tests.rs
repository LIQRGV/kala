use super::*;

#[test]
fn cumulative_summary_details_are_bounded_before_final_encoding() {
    use kala_core::overlap::{Interval, Overlap, Side};
    let mut world = World::in_memory(kala_core::Schema::new().node_type("CHARACTER")).unwrap();
    let entity = world.create_entity("CHARACTER", Some("greg")).unwrap();
    let mut result = world.begin().commit().unwrap();
    result.warnings.introduced = vec![
        Overlap {
            entity,
            attr: "alias".into(),
            side: Side::Forward,
            a: kala_core::Value::Str("a".repeat(100)),
            b: kala_core::Value::Str("b".repeat(100)),
            during: Interval {
                from: kala_core::Tick(0),
                until: None
            },
        };
        100
    ];
    let mut summary = Summary::new(2048);
    for _ in 0..256 {
        summary.add(&world, &result, &[], true);
        let optional = Value::Map(
            [
                ("entries".into(), ids(&summary.entries)),
                ("ids".into(), Value::List(summary.mapping.clone())),
                (
                    "notifications".into(),
                    Value::List(summary.notifications.clone()),
                ),
            ]
            .into(),
        );
        let retained_bytes = packstream::encode(&optional).unwrap().len();
        assert!(
            retained_bytes <= 2048,
            "summary retained {retained_bytes} bytes before final encoding despite its 2048-byte bound"
        );
    }
    let metadata = summary.metadata("varrow", false, 2048, 32);
    let retained = match metadata.get("notifications") {
        Some(Value::List(items)) => items.len(),
        None => 0,
        other => panic!("invalid notifications: {other:?}"),
    };
    let Some(Value::Map(kala)) = metadata.get("kala") else {
        panic!("missing Kala metadata")
    };
    let Some(Value::Integer(omitted)) = kala.get("notifications_omitted") else {
        panic!("missing exact omitted count: {kala:?}")
    };
    assert_eq!(retained + usize::try_from(*omitted).unwrap(), 256 * 100);
}

/// A request's deadline is carried into the executor's own cancellation
/// checks; no helper thread watches the clock. (A thread per query costs
/// every read, and `spawn` panics when the OS refuses a thread.) A thread
/// that is not started can't be observed from a test, so this pins the
/// source: the request path names no thread API.
#[test]
fn the_request_path_spawns_no_thread() {
    for (file, source) in [
        ("query.rs", include_str!("query.rs")),
        ("session.rs", include_str!("session.rs")),
        ("metadata.rs", include_str!("metadata.rs")),
    ] {
        for api in [
            "thread::scope",
            "thread::spawn",
            "thread::Builder",
            ".spawn(",
        ] {
            assert!(!source.contains(api), "{file} uses {api}");
        }
    }
}

#[test]
fn a_failure_after_committed_forms_never_says_nothing_was_written_or_invites_a_retry() {
    let lock = || store_error(Error::WorldLocked, true);
    assert_eq!(lock().code, LOCK);
    assert!(lock().message.contains("nothing was written"));
    // The first form: reported as it is, and retryable.
    let first = form_failed(lock(), 1, 0, 0, "varrow", 12);
    assert_eq!(first.code, LOCK);
    assert_eq!(first.message, lock().message);
    // A later form with nothing committed before it (the earlier forms
    // changed nothing): still true that nothing was written, still safe to
    // retry.
    let none = form_failed(lock(), 3, 0, 0, "varrow", 12);
    assert_eq!(none.code, LOCK);
    assert!(
        none.message
            .starts_with("form 3 failed; 0 prior forms committed: ")
    );
    assert!(none.message.contains("nothing was written"));
    // After commits: never "nothing was written", the last commit and its
    // bookmark are named, and no code a driver would retry (a retry of the
    // RUN would apply the committed forms a second time).
    for failure in [
        lock(),
        timeout(),
        store_error(Error::LogDiverged, true),
        Failure::new(
            CONFLICT,
            "conflict: op 1 (:set) read :greg :alias, which another transaction changed; nothing was written; run it again",
        ),
    ] {
        let was_transient = failure.code.starts_with("Neo.TransientError");
        let failed = form_failed(failure, 3, 2, 2, "varrow", 15);
        assert!(
            failed
                .message
                .starts_with("form 3 failed; 2 prior forms committed, the last as tx 15 (bookmark kala:varrow:15): "),
            "{}",
            failed.message
        );
        assert!(
            !failed.message.contains("nothing was written"),
            "{}",
            failed.message
        );
        assert!(!failed.message.contains("again"), "{}", failed.message);
        assert!(
            failed.message.contains("this form wrote nothing"),
            "{}",
            failed.message
        );
        assert!(
            !failed.code.starts_with("Neo.TransientError"),
            "{}",
            failed.code
        );
        if was_transient {
            assert_eq!(failed.code, crate::error::EXECUTION);
        }
    }
    // The failing form itself committed (its result could not be encoded):
    // a retryable code must not survive that either.
    let own = form_failed(lock(), 1, 0, 1, "varrow", 15);
    assert_eq!(own.code, crate::error::EXECUTION);
}

#[test]
fn retained_cost_of_compact_unwind_statements_counts_across_a_transaction() {
    let mut world = World::in_memory(kala_core::Schema::new().node_type("T")).unwrap();
    let front_end = kala_lang::front_end("cypher").unwrap();
    let params = kala_query::QueryParams::default();
    let statement = "UNWIND [1,2,3] AS i CREATE (:T {score:i})";
    let kala_lang::Statement::CypherWrite(stmt) =
        kala_lang::classify(front_end, statement, &kala_lang::refuse_nothing).unwrap()
    else {
        panic!("expected a write")
    };
    let mut tx = world.begin();
    let mut costs = vec![];
    for _ in 0..3 {
        let input = write::prepare_statement(
            tx.base_world(),
            &stmt,
            &params,
            kala_query::RunOptions::default(),
        )
        .unwrap();
        let staged = write::stage_statement(&mut tx, &stmt, input, &params).unwrap();
        // The expanded input creates three entities and three facts per RUN.
        assert_eq!(staged.staged.ops.len(), 6);
        assert!(staged.retained_bytes > statement.len() + 6 * "score".len());
        costs.push(staged.retained_bytes);
    }
    // Start near the real cap to exercise its boundary with three small RUNs.
    let mut total = MAX_STAGED_BYTES - costs[0] - costs[1];
    charge(&mut total, costs[0]).unwrap();
    charge(&mut total, costs[1]).unwrap();
    assert_eq!(total, MAX_STAGED_BYTES);
    assert!(charge(&mut total, costs[2]).is_err());
    assert_eq!(total, MAX_STAGED_BYTES);
}

#[test]
fn set_retention_includes_noop_and_member_payloads() {
    let mut world = World::in_memory(kala_core::Schema::new().node_type("T")).unwrap();
    world.create_entity("T", Some("n")).unwrap();
    let mut tx = world.begin();
    let fe = kala_lang::front_end("cypher").unwrap();
    let params = kala_query::QueryParams::default();
    let mut costs = vec![];
    for q in [
        "MATCH (n {kala_handle:'n'}) SET n.tags = 'a' AT TICK 0",
        "MATCH (n {kala_handle:'n'}) SET n.tags = 'a' AT TICK 1",
        "MATCH (n {kala_handle:'n'}) SET n.tags = ['a','longer member','another member'] AT TICK 2",
    ] {
        let kala_lang::Statement::CypherWrite(stmt) =
            kala_lang::classify(fe, q, &kala_lang::refuse_nothing).unwrap()
        else {
            panic!("expected a write")
        };
        let input = write::prepare_statement(
            tx.base_world(),
            &stmt,
            &params,
            kala_query::RunOptions::default(),
        )
        .unwrap();
        let result = write::stage_statement(&mut tx, &stmt, input, &params).unwrap();
        costs.push((result.staged.ops[0].changed, result.retained_bytes));
    }
    assert_eq!(costs[1].0, 0);
    assert!(costs[1].1 >= std::mem::size_of::<WriteOperation>());
    assert_eq!(costs[2].0, 2);
    assert!(costs[2].1 >= costs[0].1 + "longer member".len() + "another member".len());
}
