//! Slice 6, item 8: resource limits. `Limits { max_rows, max_steps }` are
//! errors, never truncation; any single operator (intermediate or final) may
//! hold at most `max_rows` rows; steps are counted (one per index read and
//! per row produced), never timed, so outcomes are deterministic.
//! Plans are hand-built.

mod common;

use common::*;
use kala_core::{AttrDef, Cardinality, Kind, Schema, Tick, Value, ValueType, World};
use kala_query::{Access, Limits, Params, Plan, QueryError, ResultSet, execute};

fn schema() -> Schema {
    Schema::new()
        .node_type("CHARACTER")
        .node_type("PLACE")
        .attribute(AttrDef::new(
            "n",
            ValueType::I64,
            Kind::State,
            Cardinality::One,
        ))
}

/// `chars` CHARACTERs and `places` PLACEs, each with an `n` at tick 0.
fn world(chars: usize, places: usize) -> World {
    let mut w = World::in_memory(schema()).unwrap();
    for i in 0..chars {
        let e = w.create_entity("CHARACTER", None).unwrap();
        w.assert(e, "n", Value::I64(i as i64), Some(Tick(0)))
            .unwrap();
    }
    for i in 0..places {
        let e = w.create_entity("PLACE", None).unwrap();
        w.assert(e, "n", Value::I64(i as i64), Some(Tick(0)))
            .unwrap();
    }
    w
}

fn go(w: &World, p: &Plan, lim: &Limits) -> Result<ResultSet, QueryError> {
    execute(&w.at_canon(w.latest_tx()), p, &Params::new(), lim)
}

fn steps(s: u64) -> Limits {
    Limits {
        max_steps: s,
        ..Limits::default()
    }
}

fn rows(r: usize) -> Limits {
    Limits {
        max_rows: r,
        ..Limits::default()
    }
}

/// chars x places as a Cartesian product of two node scans.
fn product() -> Plan {
    plan(
        &["x", "y"],
        join(
            node_scan(Op::Unit, "x", "CHARACTER"),
            node_scan(Op::Unit, "y", "PLACE"),
        ),
    )
}

use kala_query::Op;

// ---------------------------------------------------------------- row limit

#[test]
fn cartesian_of_three_scans_over_default_row_limit_is_row_limit_error() {
    let w = world(50, 0); // 50^3 = 125_000 > 100_000
    let p = plan(
        &["x", "y", "z"],
        join(
            join(
                node_scan(Op::Unit, "x", "CHARACTER"),
                node_scan(Op::Unit, "y", "CHARACTER"),
            ),
            node_scan(Op::Unit, "z", "CHARACTER"),
        ),
    );
    match go(&w, &p, &Limits::default()) {
        Err(QueryError::RowLimit { limit: 100_000 }) => {}
        other => panic!("expected RowLimit{{100000}}, got {other:?}"),
    }
}

#[test]
fn row_limit_error_is_prompt_measured_in_steps_not_time() {
    // The error must arrive after roughly max_rows steps of work, not after
    // building the whole product: with a step budget of 2x the row limit the
    // error is still RowLimit, not StepBudget.
    let w = world(50, 0);
    let p = plan(
        &["x", "y", "z"],
        join(
            join(
                node_scan(Op::Unit, "x", "CHARACTER"),
                node_scan(Op::Unit, "y", "CHARACTER"),
            ),
            node_scan(Op::Unit, "z", "CHARACTER"),
        ),
    );
    let lim = Limits {
        max_rows: 100_000,
        max_steps: 200_000,
        ..Limits::default()
    };
    assert!(matches!(
        go(&w, &p, &lim),
        Err(QueryError::RowLimit { limit: 100_000 })
    ));
}

#[test]
fn product_exactly_at_the_row_limit_succeeds_with_exact_count() {
    let w = world(400, 250); // 400 * 250 = 100_000
    let rs = go(&w, &product(), &Limits::default()).unwrap();
    assert_eq!(rs.rows.len(), 100_000);
}

#[test]
fn product_one_over_the_row_limit_is_row_limit_error() {
    let w = world(400, 251); // 100_400
    assert!(matches!(
        go(&w, &product(), &Limits::default()),
        Err(QueryError::RowLimit { limit: 100_000 })
    ));
}

#[test]
fn product_just_under_the_limit_succeeds() {
    let w = world(46, 46);
    let w3 = world(46, 0);
    let p = plan(
        &["x", "y", "z"],
        join(
            join(
                node_scan(Op::Unit, "x", "CHARACTER"),
                node_scan(Op::Unit, "y", "CHARACTER"),
            ),
            node_scan(Op::Unit, "z", "CHARACTER"),
        ),
    );
    assert_eq!(
        go(&w3, &p, &Limits::default()).unwrap().rows.len(),
        46 * 46 * 46
    );
    assert_eq!(
        go(&w, &product(), &Limits::default()).unwrap().rows.len(),
        46 * 46
    );
}

#[test]
fn max_rows_applies_to_intermediate_results_not_just_the_final() {
    // Final projection is exactly one (empty) row; the product under it is 100*100.
    let w = world(100, 0);
    let p = plan(
        &[],
        project(
            join(
                node_scan(Op::Unit, "x", "CHARACTER"),
                node_scan(Op::Unit, "y", "CHARACTER"),
            ),
            &[],
        ),
    );
    assert!(matches!(
        go(&w, &p, &rows(5_000)),
        Err(QueryError::RowLimit { limit: 5_000 })
    ));
    // with room it succeeds with the 1-row answer
    let rs = go(&w, &p, &rows(10_000)).unwrap();
    assert_eq!(rs.rows.len(), 1);
}

#[test]
fn max_rows_applies_to_a_filter_that_would_remove_the_product() {
    use kala_query::CmpOp;
    let w = world(100, 0);
    let p = plan(
        &["x"],
        project(
            filter(
                join(
                    node_scan(Op::Unit, "x", "CHARACTER"),
                    node_scan(Op::Unit, "y", "CHARACTER"),
                ),
                CmpOp::Eq,
                tv("x"),
                tv("y"),
            ),
            &["x"],
        ),
    );
    assert!(matches!(
        go(&w, &p, &rows(5_000)),
        Err(QueryError::RowLimit { limit: 5_000 })
    ));
    assert_eq!(go(&w, &p, &rows(10_000)).unwrap().rows.len(), 100);
}

#[test]
fn join_without_shared_vars_of_two_large_expand_scans_respects_max_rows() {
    let w = world(400, 0);
    let side = |v: &str, n: &str| expand(tv(v), "n", tv(n), TickSourceAt0(), Access::Scan);
    let p = plan(&["a", "b"], join(side("a", "m"), side("b", "k"))); // 160_000
    assert!(matches!(
        go(&w, &p, &Limits::default()),
        Err(QueryError::RowLimit { limit: 100_000 })
    ));
    // each side alone is fine
    let one = plan(&["a"], project(side("a", "m"), &["a"]));
    assert_eq!(go(&w, &one, &Limits::default()).unwrap().rows.len(), 400);
}

#[allow(non_snake_case)]
fn TickSourceAt0() -> kala_query::TickSource {
    at(0)
}

#[test]
fn row_limit_error_is_prompt_before_the_product_is_built() {
    // Two 900-entity scans fit under max_rows = 1000 individually; their
    // product (810_000) must stop at ~1000 rows: a small step budget is
    // enough to reach RowLimit, StepBudget is never hit first.
    let w = world(900, 900);
    let lim = Limits {
        max_rows: 1_000,
        max_steps: 10_000,
        ..Limits::default()
    };
    assert!(matches!(
        go(&w, &product(), &lim),
        Err(QueryError::RowLimit { limit: 1_000 })
    ));
}

#[test]
fn two_2000_entity_scans_with_row_limit_1000_error_within_100k_steps() {
    let w = world(2000, 2000);
    let lim = Limits {
        max_rows: 1_000,
        max_steps: 100_000,
        ..Limits::default()
    };
    assert!(matches!(
        go(&w, &product(), &lim),
        Err(QueryError::RowLimit { limit: 1_000 })
    ));
}

// ---------------------------------------------------------------- step budget

fn all_chars() -> Plan {
    one_col("x", node_scan(Op::Unit, "x", "CHARACTER"))
}

/// Least budget under which `p` runs (binary search; monotone by construction).
fn min_budget(w: &World, p: &Plan) -> u64 {
    let (mut lo, mut hi) = (0u64, 10_000_000u64);
    assert!(go(w, p, &steps(hi)).is_ok());
    while lo < hi {
        let mid = (lo + hi) / 2;
        if go(w, p, &steps(mid)).is_ok() {
            hi = mid;
        } else {
            lo = mid + 1;
        }
    }
    lo
}

#[test]
fn big_scan_over_step_budget_is_step_budget_error_and_larger_budget_succeeds() {
    let w = world(1000, 0);
    match go(&w, &all_chars(), &steps(500)) {
        Err(QueryError::StepBudget { limit: 500 }) => {}
        other => panic!("expected StepBudget{{500}}, got {other:?}"),
    }
    let rs = go(&w, &all_chars(), &steps(50_000)).unwrap();
    assert_eq!(rs.rows.len(), 1000);
}

#[test]
fn step_budget_outcome_is_deterministic_across_repeated_runs() {
    let w = world(300, 0);
    let p = all_chars();
    let min = min_budget(&w, &p);
    assert!(
        min >= 300,
        "reading 300 entities must cost at least 300 steps, got {min}"
    );
    for _ in 0..5 {
        assert!(
            go(&w, &p, &steps(min)).is_ok(),
            "min budget {min} must pass every time"
        );
        assert!(
            matches!(go(&w, &p, &steps(min - 1)), Err(QueryError::StepBudget { limit }) if limit == min - 1),
            "min-1 must fail every time"
        );
    }
    // and the same minimum is found again (fresh world, same data)
    assert_eq!(min_budget(&world(300, 0), &p), min);
}

#[test]
fn step_budget_never_truncates() {
    // Any budget either gives the full answer or an error.
    let w = world(200, 0);
    let p = all_chars();
    for s in [0u64, 1, 10, 100, 199, 200, 201, 250, 400, 10_000] {
        match go(&w, &p, &steps(s)) {
            Ok(rs) => assert_eq!(rs.rows.len(), 200, "budget {s} returned a partial answer"),
            Err(QueryError::StepBudget { limit }) => assert_eq!(limit, s),
            Err(e) => panic!("unexpected error at budget {s}: {e}"),
        }
    }
}

// ---------------------------------------------------------------- zero limits

#[test]
fn max_rows_zero_is_an_error_not_a_panic_for_a_query_that_produces_a_row() {
    let w = world(3, 0);
    let lim = Limits {
        max_rows: 0,
        ..Limits::default()
    };
    assert!(matches!(
        go(&w, &all_chars(), &lim),
        Err(QueryError::RowLimit { limit: 0 })
    ));
}

#[test]
fn max_rows_zero_even_a_unit_root_errors_never_panics() {
    // Every plan starts from Unit, which holds one row.
    let w = world(0, 0);
    let p = plan(&[], Op::Unit);
    let lim = Limits {
        max_rows: 0,
        ..Limits::default()
    };
    assert!(matches!(
        go(&w, &p, &lim),
        Err(QueryError::RowLimit { limit: 0 })
    ));
}

#[test]
fn max_steps_zero_is_an_error_not_a_panic() {
    let w = world(3, 0);
    let lim = Limits {
        max_steps: 0,
        ..Limits::default()
    };
    assert!(matches!(
        go(&w, &all_chars(), &lim),
        Err(QueryError::StepBudget { limit: 0 })
    ));
    assert!(matches!(
        go(&w, &plan(&[], Op::Unit), &lim),
        Err(QueryError::StepBudget { limit: 0 })
    ));
}

#[test]
fn both_limits_zero_never_panics() {
    let w = world(3, 0);
    let lim = Limits {
        max_rows: 0,
        max_steps: 0,
        ..Limits::default()
    };
    assert!(go(&w, &all_chars(), &lim).is_err());
}

#[test]
fn max_rows_usize_max_and_max_steps_u64_max_do_not_overflow() {
    let w = world(10, 0);
    let lim = Limits {
        max_rows: usize::MAX,
        max_steps: u64::MAX,
        ..Limits::default()
    };
    assert_eq!(go(&w, &all_chars(), &lim).unwrap().rows.len(), 10);
}

// ---------------------------------------------------------------- Display

#[test]
fn errors_display_the_limit_number() {
    let w = world(50, 0);
    let e = go(&w, &all_chars(), &rows(7)).unwrap_err();
    assert!(e.to_string().contains('7'), "RowLimit display: {e}");
    let w = world(50, 0);
    let e = go(&w, &all_chars(), &steps(45)).unwrap_err();
    assert!(e.to_string().contains("45"), "StepBudget display: {e}");
    let e = QueryError::RowLimit { limit: 100_000 };
    assert!(e.to_string().contains("100000"), "{e}");
    let e = QueryError::StepBudget { limit: 10_000_000 };
    assert!(e.to_string().contains("10000000"), "{e}");
}
