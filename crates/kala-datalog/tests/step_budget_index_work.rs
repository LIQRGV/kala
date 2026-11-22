//! Slice 7, item 00 (b), M1: the step budget charges index work (seeks and
//! scanned keys inside kala-core reads), not only result rows. The executor
//! must charge at least 1 step per index key examined.
//!
//! These compile today and fail on assertions (the bug: a scan over a huge
//! world with a tiny answer is nearly free).

use kala_core::*;
use kala_datalog::prepare;
use kala_query::{Limits, Params, QueryError, ResultSet, run};

fn go(w: &World, q: &str, lim: &Limits) -> Result<ResultSet, QueryError> {
    let p = prepare(w, q).unwrap_or_else(|e| panic!("prepare failed: {e}"));
    run(w, &p.plan, &Params::new(), lim)
}

fn steps(n: u64) -> Limits {
    Limits {
        max_steps: n,
        ..Limits::default()
    }
}

/// 20,000 defined entities, exactly one with a `rare` fact.
fn wide_world() -> World {
    let schema = Schema::new().node_type("CHARACTER").attribute(AttrDef::new(
        "rare",
        ValueType::I64,
        Kind::State,
        Cardinality::One,
    ));
    let mut w = World::in_memory(schema).unwrap();
    let mut t = w.begin();
    let mut last = None;
    for _ in 0..20_000 {
        last = Some(t.create_entity("CHARACTER", None).unwrap());
    }
    t.assert(last.unwrap(), "rare", Value::I64(7), Some(Tick(0)))
        .unwrap();
    t.commit().unwrap();
    w
}

const SCAN: &str = "[:find ?e ?v :where [?e :rare ?v]]";

#[test]
fn scan_over_many_defined_ids_exhausts_a_small_step_budget() {
    let w = wide_world();
    match go(&w, SCAN, &steps(1_000)) {
        Err(QueryError::StepBudget { limit: 1_000 }) => {}
        other => panic!(
            "scan visits ~20,000 IDs; expected StepBudget under 1,000 steps, got {:?}",
            other.map(|r| r.rows)
        ),
    }
}

#[test]
fn the_same_scan_succeeds_under_default_limits() {
    let w = wide_world();
    let rs = go(&w, SCAN, &Limits::default()).expect("default limits suffice");
    assert_eq!(rs.rows.len(), 1);
}

/// One entity that has held 5,000 distinct tags, all retracted by the latest
/// tick: the answer is empty, but a history-free `AsOf(entity, attr)` walks
/// every distinct value ever held.
fn tagged_world() -> World {
    let schema = Schema::new().node_type("CHARACTER").attribute(AttrDef::new(
        "tag",
        ValueType::I64,
        Kind::State,
        Cardinality::Many,
    ));
    let mut w = World::in_memory(schema).unwrap();
    let mut t = w.begin();
    let x = t.create_entity("CHARACTER", Some("hero")).unwrap();
    for i in 0..5_000 {
        t.assert(x, "tag", Value::I64(i), Some(Tick(0))).unwrap();
    }
    for i in 0..5_000 {
        t.retract(x, "tag", Value::I64(i), Some(Tick(1))).unwrap();
    }
    t.commit().unwrap();
    w
}

const TAGS: &str = "[:find ?v :where [:hero :tag ?v]]";

#[test]
fn e_read_walking_thousands_of_distinct_values_exhausts_a_small_step_budget() {
    let w = tagged_world();
    match go(&w, TAGS, &steps(1_000)) {
        Err(QueryError::StepBudget { limit: 1_000 }) => {}
        other => panic!(
            "walking 5,000 values (10,000 keys) must cost steps; got {:?}",
            other.map(|r| r.rows)
        ),
    }
}

#[test]
fn the_same_e_read_succeeds_under_default_limits_with_no_rows() {
    let w = tagged_world();
    let rs = go(&w, TAGS, &Limits::default()).expect("default limits suffice");
    assert!(
        rs.rows.is_empty(),
        "every tag is retracted at the latest tick"
    );
}
