//! Slice 7 security finding H1 (RED): the executor's row-set operations
//! compare/hash whole `Str` / `IntervalSet` contents, unmetered. Contract:
//! O(1) per comparison/hash regardless of value length; the final sort's
//! comparison work on long values is charged to the step budget (about 1 step
//! per 64 bytes compared, per 4 intervals). The 30 s guard is a hang guard,
//! not a timing assertion.

use kala_core::*;
use kala_datalog::prepare;
use kala_query::{Limits, Params, QueryError, ResultSet, run};
use std::sync::mpsc;
use std::time::Duration;

const GUARD: Duration = Duration::from_secs(30);

fn schema() -> Schema {
    Schema::new()
        .node_type("CHARACTER")
        .attribute(AttrDef::new("bio", ValueType::Str, Kind::State, Cardinality::Many).unindexed())
        .attribute(AttrDef::new(
            "p",
            ValueType::Ref,
            Kind::State,
            Cardinality::Many,
        ))
        .attribute(AttrDef::new(
            "q",
            ValueType::I64,
            Kind::State,
            Cardinality::Many,
        ))
}

/// Adds `n` entities each with `q = i`, in one transaction.
fn add_q_rows(w: &mut World, n: usize) {
    let mut t = w.begin();
    for i in 0..n {
        let e = t.create_entity("CHARACTER", None).unwrap();
        t.assert(e, "q", Value::I64(i as i64), Some(Tick(0)))
            .unwrap();
    }
    t.commit().unwrap();
}

/// Builds the world in the guarded thread too, then runs `q`.
fn guarded<F>(build: F, q: &'static str, lim: Limits) -> Result<ResultSet, QueryError>
where
    F: FnOnce() -> World + Send + 'static,
{
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let w = build();
        let p = prepare(&w, q).unwrap_or_else(|e| panic!("prepare failed: {e}"));
        let _ = tx.send(run(&w, &p.plan, &Params::new(), &lim));
    });
    match rx.recv_timeout(GUARD) {
        Ok(r) => r,
        Err(mpsc::RecvTimeoutError::Timeout) => {
            panic!("HANG GUARD: query still running after {GUARD:?} (H1)")
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            panic!("worker thread died (world build or prepare failed), not a hang (H1)")
        }
    }
}

fn count_of(rs: &ResultSet) -> String {
    assert_eq!(rs.rows.len(), 1);
    format!("{:?}", rs.rows[0][0])
}

const ROWS: usize = 60_000;

fn world_t1() -> World {
    let mut w = World::in_memory(schema()).unwrap();
    let a = w.create_entity("CHARACTER", Some("a")).unwrap();
    w.assert(a, "bio", Value::Str("x".repeat(1024 * 1024)), Some(Tick(0)))
        .unwrap();
    add_q_rows(&mut w, ROWS);
    w
}

#[test]
fn t1_one_mb_string_times_60k_rows_completes() {
    let rs = guarded(
        world_t1,
        "[:find (count ?v) :where [:a :bio ?s] [?x :q ?v]]",
        Limits::default(),
    )
    .expect("Ok under default limits");
    assert!(count_of(&rs).contains("60000"), "{:?}", rs.rows);
}

fn world_t2() -> World {
    let mut w = World::in_memory(schema()).unwrap();
    let a = w.create_entity("CHARACTER", Some("a")).unwrap();
    let b = w.create_entity("CHARACTER", Some("b")).unwrap();
    let mut t = w.begin();
    for i in 0..20_000i64 {
        if i % 2 == 0 {
            t.assert(a, "p", Value::Ref(b), Some(Tick(i))).unwrap();
        } else {
            t.retract(a, "p", Value::Ref(b), Some(Tick(i))).unwrap();
        }
    }
    t.commit().unwrap();
    add_q_rows(&mut w, ROWS);
    w
}

#[test]
fn t2_ten_thousand_interval_set_times_60k_rows_completes() {
    let rs = guarded(
        world_t2,
        "[:find (count ?v) :at 0 :where (during [:a :p :b] ?s) [?x :q ?v]]",
        Limits::default(),
    )
    .expect("Ok under default limits");
    assert!(count_of(&rs).contains("60000"), "{:?}", rs.rows);
}

/// `n` distinct strings of `len` bytes sharing all but a 4-digit suffix, all on
/// one entity (bio is cardinality many, so rows differ first in ?s), plus
/// `q_rows` q rows.
fn world_long_strings(n: usize, len: usize, q_rows: usize) -> World {
    let mut w = World::in_memory(schema()).unwrap();
    let prefix = "p".repeat(len - 4);
    let a = w.create_entity("CHARACTER", Some("a")).unwrap();
    let mut t = w.begin();
    for i in 0..n {
        let s = format!("{prefix}{i:04}");
        t.assert(a, "bio", Value::Str(s), Some(Tick(0))).unwrap();
    }
    t.commit().unwrap();
    add_q_rows(&mut w, q_rows);
    w
}

#[test]
fn t3_64_distinct_1mb_strings_cross_1500_rows_count_completes() {
    let rs = guarded(
        || world_long_strings(64, 1024 * 1024, 1_500),
        "[:find (count ?v) :where [:a :bio ?s] [?x :q ?v]]",
        Limits::default(),
    )
    .expect("Ok under default limits");
    // Rows are a set of (?e, ?s, ?x, ?v) before the aggregate: 500 x 200 rows,
    // count counts rows: 64 x 1500 = 96000.
    assert!(count_of(&rs).contains("96000"), "{:?}", rs.rows);
}

#[test]
fn t4_final_result_of_500_long_strings_ok_by_default() {
    let rs = guarded(
        || world_long_strings(500, 32 * 1024, 0),
        "[:find ?s :where [:a :bio ?s]]",
        Limits::default(),
    )
    .expect("Ok under default limits");
    assert_eq!(rs.rows.len(), 500);
}

#[test]
fn t4_final_sort_comparisons_are_charged_to_the_step_budget() {
    // Everything except the final sort costs well under 200_000 steps (500
    // scan rows); the sort compares ~500*log2(500) pairs of ~31 KB prefixes,
    // ~2.2M steps at 1 step per 64 bytes.
    let lim = Limits {
        max_steps: 200_000,
        ..Limits::default()
    };
    let r = guarded(
        || world_long_strings(500, 32 * 1024, 0),
        "[:find ?s :where [:a :bio ?s]]",
        lim,
    );
    assert!(
        matches!(r, Err(QueryError::StepBudget { limit: 200_000 })),
        "expected StepBudget{{200000}}, got {:?}",
        r.map(|r| r.rows.len())
    );
}
