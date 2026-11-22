//! Slice 6, item 8: resource limits through Datalog (`prepare` + `run`).
//! Limits are errors, never truncation; max_rows applies to intermediate
//! results; steps are counted, not timed.

use kala_core::*;
use kala_datalog::prepare;
use kala_query::{Limits, Params, QueryError, ResultSet, run};

/// `n` CHARACTERs, each with n1, n2, n3 (distinct i64s) and the same `tag`.
fn world(n: usize) -> World {
    let schema = Schema::new()
        .node_type("CHARACTER")
        .attribute(AttrDef::new(
            "n1",
            ValueType::I64,
            Kind::State,
            Cardinality::One,
        ))
        .attribute(AttrDef::new(
            "n2",
            ValueType::I64,
            Kind::State,
            Cardinality::One,
        ))
        .attribute(AttrDef::new(
            "n3",
            ValueType::I64,
            Kind::State,
            Cardinality::One,
        ))
        .attribute(AttrDef::new(
            "tag",
            ValueType::Str,
            Kind::State,
            Cardinality::One,
        ));
    let mut w = World::in_memory(schema).unwrap();
    for i in 0..n {
        let e = w.create_entity("CHARACTER", None).unwrap();
        let t = Some(Tick(0));
        for a in ["n1", "n2", "n3"] {
            w.assert(e, a, Value::I64(i as i64), t).unwrap();
        }
        w.assert(e, "tag", Value::Str("k".into()), t).unwrap();
    }
    w
}

const THREE: &str = "[:find ?a ?b ?c :where [?a :n1 ?x] [?b :n2 ?y] [?c :n3 ?z]]";

fn go(w: &World, q: &str, lim: &Limits) -> Result<ResultSet, QueryError> {
    let p = prepare(w, q).unwrap_or_else(|e| panic!("prepare failed: {e}"));
    run(w, &p.plan, &Params::new(), lim)
}

#[test]
fn three_unrelated_scans_over_default_row_limit_is_row_limit_error() {
    let w = world(50); // 125_000 > 100_000
    match go(&w, THREE, &Limits::default()) {
        Err(QueryError::RowLimit { limit: 100_000 }) => {}
        other => panic!(
            "expected RowLimit{{100000}}, got {:?}",
            other.map(|r| r.rows.len())
        ),
    }
}

#[test]
fn three_unrelated_scans_error_promptly_in_steps() {
    // The planner emits nested-loop scans, and every repeated scan read is
    // charged (probed: RowLimit needs between 200_000 and 1_000_000 steps).
    // The full product would cost ~125_000 x 50 reads, so 1_000_000 steps
    // only reach RowLimit if the error stops the work early.
    let w = world(50);
    let lim = Limits {
        max_rows: 100_000,
        max_steps: 1_000_000,
    };
    assert!(matches!(
        go(&w, THREE, &lim),
        Err(QueryError::RowLimit { limit: 100_000 })
    ));
}

#[test]
fn three_unrelated_scans_just_under_the_limit_succeed_with_exact_count() {
    let w = world(46); // 97_336
    let rs = go(&w, THREE, &Limits::default()).unwrap();
    assert_eq!(rs.rows.len(), 46 * 46 * 46);
}

#[test]
fn intermediate_over_max_rows_with_one_row_final_is_row_limit() {
    // Every entity has tag "k": the final answer is one row (?t = "k"), but
    // the product of the three scans under it is 20^3 = 8000 rows.
    let w = world(20);
    let q = "[:find ?t :where [?a :n1 ?x] [?b :n2 ?y] [?c :n3 ?z] [?a :tag ?t]]";
    let ok = go(&w, q, &Limits::default()).unwrap();
    assert_eq!(ok.rows.len(), 1);
    let lim = Limits {
        max_rows: 1_000,
        ..Limits::default()
    };
    assert!(
        matches!(go(&w, q, &lim), Err(QueryError::RowLimit { limit: 1_000 })),
        "the 1-row answer must not hide an over-limit intermediate"
    );
}

#[test]
fn big_scan_over_step_budget_is_step_budget_error_and_larger_succeeds() {
    let w = world(500);
    let q = "[:find ?a :where [?a :n1 ?x]]";
    let small = Limits {
        max_steps: 100,
        ..Limits::default()
    };
    assert!(matches!(
        go(&w, q, &small),
        Err(QueryError::StepBudget { limit: 100 })
    ));
    let big = Limits {
        max_steps: 100_000,
        ..Limits::default()
    };
    assert_eq!(go(&w, q, &big).unwrap().rows.len(), 500);
}

#[test]
fn step_budget_outcome_is_deterministic_across_runs() {
    let w = world(200);
    let q = "[:find ?a :where [?a :n1 ?x] [?a :n2 ?y]]";
    let ok = |s: u64| {
        go(
            &w,
            q,
            &Limits {
                max_steps: s,
                ..Limits::default()
            },
        )
        .is_ok()
    };
    let (mut lo, mut hi) = (0u64, 1_000_000u64);
    assert!(ok(hi));
    while lo < hi {
        let mid = (lo + hi) / 2;
        if ok(mid) { hi = mid } else { lo = mid + 1 }
    }
    for _ in 0..5 {
        assert!(ok(lo), "min budget {lo} passes every time");
        assert!(!ok(lo - 1), "min budget - 1 fails every time");
    }
}

#[test]
fn zero_limits_error_without_panic() {
    let w = world(3);
    let q = "[:find ?a :where [?a :n1 ?x]]";
    let r = Limits {
        max_rows: 0,
        ..Limits::default()
    };
    assert!(matches!(
        go(&w, q, &r),
        Err(QueryError::RowLimit { limit: 0 })
    ));
    let s = Limits {
        max_steps: 0,
        ..Limits::default()
    };
    assert!(matches!(
        go(&w, q, &s),
        Err(QueryError::StepBudget { limit: 0 })
    ));
}

#[test]
fn limit_errors_name_the_limit_in_display() {
    let w = world(50);
    let e = go(
        &w,
        "[:find ?a :where [?a :n1 ?x]]",
        &Limits {
            max_rows: 7,
            ..Limits::default()
        },
    )
    .unwrap_err();
    assert!(e.to_string().contains('7'), "{e}");
    let e = go(
        &w,
        "[:find ?a :where [?a :n1 ?x]]",
        &Limits {
            max_steps: 13,
            ..Limits::default()
        },
    )
    .unwrap_err();
    assert!(e.to_string().contains("13"), "{e}");
}
