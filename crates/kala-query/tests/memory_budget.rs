//! Slice 7, item 00 (a), M2: a per-query byte budget, and rows that share
//! long strings instead of copying them.
//!
//! Expected API (RED until it exists; a compile error on these is the red):
//!   `Limits { max_bytes: usize, .. }`, `Default` = 256 * 1024 * 1024;
//!   `QueryError::MemoryBudget { limit: usize }`, Display contains "bytes".
//!
//! Margins are generous on purpose: only the order of magnitude is pinned.

mod common;

use common::*;
use kala_core::{AttrDef, Cardinality, Kind, Schema, Tick, Value, ValueType, World};
use kala_query::{Access, CmpOp, Limits, Op, Params, QueryError, run};

const MIB: usize = 1024 * 1024;

fn schema() -> Schema {
    Schema::new()
        .node_type("CHARACTER")
        .attribute(AttrDef::new("bio", ValueType::Str, Kind::State, Cardinality::One).unindexed())
        .attribute(AttrDef::new(
            "n",
            ValueType::I64,
            Kind::State,
            Cardinality::One,
        ))
}

fn big_limits(max_bytes: usize) -> Limits {
    Limits {
        max_rows: 1_000_000,
        max_steps: 100_000_000,
        max_bytes,
    }
}

#[test]
fn default_max_bytes_is_256_mib() {
    assert_eq!(Limits::default().max_bytes, 256 * MIB);
}

#[test]
fn memory_budget_error_mentions_bytes() {
    let text = QueryError::MemoryBudget { limit: 1234 }.to_string();
    assert!(text.contains("bytes"), "{text}");
    assert!(text.contains("1234"), "{text}");
}

/// B1: ~2,000 rows with a distinct ~1 KB string each (~2 MB of payload).
#[test]
fn b1_final_result_over_budget_fails_and_under_64_mib_succeeds() {
    let mut w = World::in_memory(schema()).unwrap();
    for i in 0..2000 {
        let e = w.create_entity("CHARACTER", None).unwrap();
        let s = format!("{i:05}{}", "x".repeat(1019));
        w.assert(e, "bio", Value::Str(s), Some(Tick(0))).unwrap();
    }
    let p = plan(
        &["e", "s"],
        expand(tv("e"), "bio", tv("s"), at(0), Access::Scan),
    );
    match run(&w, &p, &Params::new(), &big_limits(MIB)) {
        Err(QueryError::MemoryBudget { .. }) => {}
        other => panic!(
            "expected MemoryBudget under 1 MiB, got {:?}",
            other.map(|r| r.rows.len())
        ),
    }
    let rs = run(&w, &p, &Params::new(), &big_limits(64 * MIB)).expect("fits in 64 MiB");
    assert_eq!(rs.rows.len(), 2000);
}

/// B2: one 100 KB string joined with 5,000 rows, then projected away. A
/// per-row copy would need ~500 MB; sharing needs a few hundred KB.
#[test]
fn b2_long_string_is_shared_across_joined_rows() {
    let mut w = World::in_memory(schema()).unwrap();
    let bio = w.create_entity("CHARACTER", None).unwrap();
    w.assert(bio, "bio", Value::Str("b".repeat(100_000)), Some(Tick(0)))
        .unwrap();
    for i in 0..5000 {
        let e = w.create_entity("CHARACTER", None).unwrap();
        w.assert(e, "n", Value::I64(i), Some(Tick(0))).unwrap();
    }
    let left = expand(tv("b"), "bio", tv("s"), at(0), Access::Scan);
    let right = expand(tv("x"), "n", tv("y"), at(0), Access::Scan);
    let p = plan(&["x"], project(join(left, right), &["x"]));
    let rs = run(&w, &p, &Params::new(), &big_limits(4 * MIB))
        .unwrap_or_else(|e| panic!("sharing should keep this under 4 MiB: {e}"));
    assert_eq!(rs.rows.len(), 5000);
}

/// B3: an intermediate set of 90,000 rows x 4 columns, final result 1 row.
#[test]
fn b3_intermediate_sets_count_against_the_budget() {
    let mut w = World::in_memory(schema()).unwrap();
    for i in 0..300 {
        let e = w.create_entity("CHARACTER", None).unwrap();
        w.assert(e, "n", Value::I64(i), Some(Tick(0))).unwrap();
    }
    let a = expand(tv("a"), "n", tv("x"), at(0), Access::Scan);
    let b = expand(tv("b"), "n", tv("y"), at(0), Access::Scan);
    let prod = join(a, b); // 300 x 300 = 90,000 rows of (a, x, b, y)
    let f1 = filter(prod, CmpOp::Eq, tv("x"), ti(0));
    let f2 = filter(f1, CmpOp::Eq, tv("y"), ti(0));
    let p = plan(&["a"], project(f2, &["a"]));
    // Control: it is a perfectly good query under the default budget.
    let rs = run(
        &w,
        &p,
        &Params::new(),
        &big_limits(Limits::default().max_bytes),
    )
    .expect("runs under the default byte budget");
    assert_eq!(rs.rows.len(), 1);
    match run(&w, &p, &Params::new(), &big_limits(MIB)) {
        Err(QueryError::MemoryBudget { .. }) => {}
        other => panic!(
            "expected MemoryBudget under 1 MiB, got {:?}",
            other.map(|r| r.rows.len())
        ),
    }
    let _ = Op::Unit;
}
