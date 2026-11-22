//! Slice 7 security finding H3 (RED): reading a bound long string value at a
//! per-row tick (`as_of_value`) or per-row entity (`holds_during`) is O(len)
//! per row but charged ~1 step, and the per-query memo (keyed entity, value,
//! tick) never hits. Contract: on a memo miss, a read with a bound long
//! string value charges about len/64 steps (times 1 + index keys examined),
//! like re-reading an interned string. Under `Limits::default()` the repro
//! stops with `StepBudget` quickly; short strings and memo hits stay cheap.
//! The 30 s guard is a hang guard, not a timing assertion. A panicked world
//! build / prepare is reported as such, not as a hang.

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
}

/// :a has one unindexed `bio` of `bio_len` bytes; :b has `ROWS` values of `q`.
fn world_h3(bio_len: usize) -> World {
    let mut w = World::in_memory(schema()).unwrap();
    let a = w.create_entity("CHARACTER", Some("a")).unwrap();
    let b = w.create_entity("CHARACTER", Some("b")).unwrap();
    w.assert(a, "bio", Value::Str("x".repeat(bio_len)), Some(Tick(0)))
        .unwrap();
    let mut t = w.begin();
    for i in 0..ROWS {
        t.assert(b, "q", Value::I64(i as i64), Some(Tick(0)))
            .unwrap();
    }
    t.commit().unwrap();
    w
}

/// `n` entities e0.. each with `q = i` and the same `bio` of `len` bytes.
fn world_shared(n: usize, len: usize) -> World {
    let mut w = World::in_memory(schema()).unwrap();
    let a = w.create_entity("CHARACTER", Some("a")).unwrap();
    w.assert(a, "bio", Value::Str("x".repeat(len)), Some(Tick(0)))
        .unwrap();
    for i in 0..n {
        let mut t = w.begin();
        let e = t
            .create_entity("CHARACTER", Some(&format!("e{i}")))
            .unwrap();
        t.assert(e, "bio", Value::Str("x".repeat(len)), Some(Tick(0)))
            .unwrap();
        t.assert(e, "q", Value::I64(i as i64), Some(Tick(0)))
            .unwrap();
        t.commit().unwrap();
    }
    w
}

fn guarded<F>(build: F, q: &'static str, limits: Limits) -> Result<ResultSet, QueryError>
where
    F: FnOnce() -> World + Send + 'static,
{
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let w = build();
        let p = prepare(&w, q).unwrap_or_else(|e| panic!("prepare failed: {e}"));
        let _ = tx.send(run(&w, &p.plan, &Params::new(), &limits));
    });
    match rx.recv_timeout(GUARD) {
        Ok(r) => r,
        Err(mpsc::RecvTimeoutError::Timeout) => {
            panic!("HANG GUARD: query still running after {GUARD:?} (H3)")
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            panic!("worker thread died (world build or prepare failed), not a hang (H3)")
        }
    }
}

fn count(r: ResultSet) -> String {
    assert_eq!(r.rows.len(), 1);
    format!("{:?}", r.rows[0][0])
}

#[test]
fn t1_per_row_tick_read_of_long_bound_value_hits_step_budget() {
    let r = guarded(
        || world_h3(1024 * 1024),
        "[:find (count ?t) :where [:a :bio ?s] [:b :q ?t] (at ?t [:a :bio ?s])]",
        Limits::default(),
    );
    assert!(
        matches!(r, Err(QueryError::StepBudget { limit: 10_000_000 })),
        "expected StepBudget{{10_000_000}}, got {:?}",
        r.map(|rs| rs.rows)
    );
}

#[test]
fn t2_same_query_with_short_string_still_ok() {
    let r = guarded(
        || world_h3(10),
        "[:find (count ?t) :where [:a :bio ?s] [:b :q ?t] (at ?t [:a :bio ?s])]",
        Limits::default(),
    )
    .expect("Ok under default limits");
    assert!(count(r).contains("60000"));
}

/// 200 entities x 256 KB identical strings: each `(during [?e :bio ?s] ..)` is
/// a distinct memo key. Real time is small at this size, so this asserts the
/// charge: contract >= 200 * 256KB/64 = 819_200 steps, so max_steps = 400_000
/// must stop it (today it takes only a few thousand steps and returns Ok).
#[test]
fn t3_holds_during_miss_with_long_shared_value_is_charged_by_length() {
    let limits = Limits {
        max_steps: 400_000,
        ..Limits::default()
    };
    let r = guarded(
        || world_shared(200, 256 * 1024),
        "[:find (count ?e) :where [:a :bio ?s] [?e :q ?x] (during [?e :bio ?s] ?w)]",
        limits,
    );
    assert!(
        matches!(r, Err(QueryError::StepBudget { limit: 400_000 })),
        "expected StepBudget{{400_000}}, got {:?}",
        r.map(|rs| rs.rows)
    );
}

#[test]
fn t4_long_string_read_at_constant_tick_per_row_stays_ok() {
    let r = guarded(
        || world_h3(1024 * 1024),
        "[:find (count ?t) :where [:b :q ?t] (at 0 [:a :bio ?s]) (at 0 [:a :bio ?s])]",
        Limits::default(),
    )
    .expect("memo hits: Ok under default limits");
    assert!(count(r).contains("60000"));
}
