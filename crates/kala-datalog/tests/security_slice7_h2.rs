//! Slice 7 security finding H2 (RED): any executor operator that reads a world
//! string per input row is O(len) per row but charged ~1 step (the String is
//! copied out of kala-core and re-hashed by the interner each row). Contract:
//! per-row reads are memoized per query (E/as_of reads keyed by entity, attr,
//! tick; bound-value reads by entity, attr, value, tick; history/during reads
//! by entity, attr), memo memory is charged to the byte budget, and reads
//! returning an already-interned long string charge about len/64 steps. Each
//! query below must return Ok with the right count under `Limits::default()`.
//! The 30 s guard is a hang guard, not a timing assertion.
//! (H2's Low item needs 2^32 distinct values: not directly testable, skipped.)

use kala_core::*;
use kala_datalog::prepare;
use kala_query::{Limits, Params, QueryError, ResultSet, run};
use std::sync::mpsc;
use std::time::Duration;

const GUARD: Duration = Duration::from_secs(30);
const ROWS: usize = 60_000;

fn schema() -> Schema {
    Schema::new()
        .node_type("CHARACTER")
        .attribute(AttrDef::new("bio", ValueType::Str, Kind::State, Cardinality::Many).unindexed())
        .attribute(AttrDef::new(
            "q",
            ValueType::I64,
            Kind::State,
            Cardinality::Many,
        ))
        .attribute(AttrDef::new(
            "p",
            ValueType::Ref,
            Kind::State,
            Cardinality::Many,
        ))
}

/// :a has one 1 MB unindexed `bio`; :b has `ROWS` values of `q`.
fn world_h2() -> World {
    let mut w = World::in_memory(schema()).unwrap();
    let a = w.create_entity("CHARACTER", Some("a")).unwrap();
    let b = w.create_entity("CHARACTER", Some("b")).unwrap();
    w.assert(a, "bio", Value::Str("x".repeat(1024 * 1024)), Some(Tick(0)))
        .unwrap();
    let mut t = w.begin();
    for i in 0..ROWS {
        t.assert(b, "q", Value::I64(i as i64), Some(Tick(0)))
            .unwrap();
    }
    t.commit().unwrap();
    w
}

/// A chain n0 -p-> n1 -> ... -> n`len`, plus :a with a (MAX_ENTRY_BYTES - 1024)-byte bio (the fixpoint is capped at 100k rows by
/// default, so a bigger string is what makes per-row copies costly).
fn world_chain(len: usize) -> World {
    let mut w = World::in_memory(schema()).unwrap();
    let a = w.create_entity("CHARACTER", Some("a")).unwrap();
    w.assert(
        a,
        "bio",
        Value::Str("x".repeat(MAX_ENTRY_BYTES - 1024)),
        Some(Tick(0)),
    )
    .unwrap();
    let mut t = w.begin();
    let ids: Vec<_> = (0..=len)
        .map(|i| {
            t.create_entity("CHARACTER", Some(&format!("n{i}")))
                .unwrap()
        })
        .collect();
    for i in 0..len {
        t.assert(ids[i], "p", Value::Ref(ids[i + 1]), Some(Tick(0)))
            .unwrap();
    }
    t.commit().unwrap();
    w
}

fn guarded<F>(build: F, q: &'static str) -> Result<ResultSet, QueryError>
where
    F: FnOnce() -> World + Send + 'static,
{
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let w = build();
        let p = prepare(&w, q).unwrap_or_else(|e| panic!("prepare failed: {e}"));
        let _ = tx.send(run(&w, &p.plan, &Params::new(), &Limits::default()));
    });
    match rx.recv_timeout(GUARD) {
        Ok(r) => r,
        Err(mpsc::RecvTimeoutError::Timeout) => {
            panic!("HANG GUARD: query still running after {GUARD:?} (H2)")
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            panic!("worker thread died (world build or prepare failed), not a hang (H2)")
        }
    }
}

fn assert_count(r: Result<ResultSet, QueryError>, want: &str) {
    let rs = r.expect("Ok under default limits");
    assert_eq!(rs.rows.len(), 1);
    let got = format!("{:?}", rs.rows[0][0]);
    assert!(got.contains(want), "expected {want}, got {:?}", rs.rows);
}

#[test]
fn t1_e_read_of_long_string_inside_q_expand_completes() {
    assert_count(
        guarded(
            world_h2,
            "[:find (count ?v) :where [:b :q ?v] [:a :bio ?s]]",
        ),
        "60000",
    );
}

#[test]
fn t2_bound_value_read_of_long_string_per_row_completes() {
    assert_count(
        guarded(
            world_h2,
            "[:find (count ?v) :where [:a :bio ?s] [:b :q ?v] [:a :bio ?s]]",
        ),
        "60000",
    );
}

#[test]
fn t3_per_row_history_of_long_string_completes() {
    assert_count(
        guarded(
            world_h2,
            "[:find (count ?v) :where [:b :q ?v] [:a :bio ?s ?t]]",
        ),
        "60000",
    );
}

#[test]
fn t4_per_row_during_of_long_string_completes() {
    assert_count(
        guarded(
            world_h2,
            "[:find (count ?v) :where [:b :q ?v] (during [:a :bio ?s] ?w)]",
        ),
        "60000",
    );
}

#[test]
fn t5_fixpoint_body_reading_long_string_per_row_completes() {
    // chain of 400: reach has 400*401/2 = 80200 pairs, each row reads :a's bio.
    let rs = guarded(
        || world_chain(400),
        "[:find ?x ?y :rules [[(reach ?x ?y) [?x :p ?y] [:a :bio ?s]] \
         [(reach ?x ?z) (reach ?x ?y) [?y :p ?z] [:a :bio ?s]]] \
         :where (reach ?x ?y)]",
    )
    .expect("Ok under default limits");
    assert_eq!(rs.rows.len(), 80200);
}
