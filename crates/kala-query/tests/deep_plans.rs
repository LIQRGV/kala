//! Deep plans: the executor is a flat step program, so plan depth costs heap,
//! not native stack. Plans nested deeper than `MAX_PLAN_DEPTH` (operators on
//! the longest root-to-leaf path, root and `Unit` included) are refused with
//! `QueryError::Plan("plan too deep ...")`. `explain` and `Drop` are
//! iterative too. Every test body runs on a thread with a 2 MiB stack.

mod common;

use common::*;
use kala_core::{AttrDef, Cardinality, EntityId, Kind, Schema, Tick, Value, ValueType, World};
use kala_query::{
    Access, Limits, MAX_EXPLAIN_BYTES, MAX_PLAN_DEPTH, Op, Params, Plan, QueryError, ResultSet,
    execute,
};

fn on_small_stack<F: FnOnce() + Send + 'static>(f: F) {
    std::thread::Builder::new()
        .stack_size(2 * 1024 * 1024)
        .spawn(f)
        .expect("spawn")
        .join()
        .expect("test body panicked (stack overflow aborts the process instead)");
}

fn world() -> (World, EntityId) {
    let schema = Schema::new().node_type("CHARACTER").attribute(AttrDef::new(
        "n",
        ValueType::I64,
        Kind::State,
        Cardinality::One,
    ));
    let mut w = World::in_memory(schema).unwrap();
    let e = w.create_entity("CHARACTER", Some("e")).unwrap();
    w.assert(e, "n", Value::I64(7), Some(Tick(0))).unwrap();
    (w, e)
}

fn go(w: &World, p: &Plan, lim: &Limits) -> Result<ResultSet, QueryError> {
    execute(&w.at_canon(w.latest_tx()), p, &Params::new(), lim)
}

/// `Expand(e, n, ?x)` over `Unit`: 2 operators, binds `?x`.
fn leaf(e: EntityId) -> Op {
    expand(tr(e), "n", tv("x"), at(0), Access::E)
}

/// An `Expand` chain of exactly `nodes` operators (the last is over `Unit`
/// and binds `?x`; the rest re-check the same fact), counting `Unit`.
fn expand_chain(e: EntityId, nodes: usize) -> Op {
    assert!(nodes >= 2);
    let mut op = leaf(e);
    for _ in 0..nodes - 2 {
        op = expand_on(op, tr(e), "n", tv("x"), at(0), Access::E);
    }
    op
}

fn is_too_deep(r: &Result<ResultSet, QueryError>) -> bool {
    matches!(r, Err(QueryError::Plan(m)) if m.contains("too deep"))
}

#[test]
fn max_plan_depth_is_4096() {
    assert_eq!(MAX_PLAN_DEPTH, 4096);
}

#[test]
fn expand_chain_of_exactly_max_depth_runs_with_same_rows_as_two_node_plan() {
    on_small_stack(|| {
        let (w, e) = world();
        let small = go(&w, &plan(&["x"], leaf(e)), &Limits::default()).unwrap();
        assert_eq!(small.rows, vec![vec![Value::I64(7)]]);
        let deep = go(
            &w,
            &plan(&["x"], expand_chain(e, MAX_PLAN_DEPTH)),
            &Limits::default(),
        )
        .unwrap();
        assert_eq!(deep.rows, small.rows);
    });
}

#[test]
fn expand_chain_one_over_max_depth_is_plan_error_too_deep() {
    on_small_stack(|| {
        let (w, e) = world();
        let r = go(
            &w,
            &plan(&["x"], expand_chain(e, MAX_PLAN_DEPTH + 1)),
            &Limits::default(),
        );
        assert!(is_too_deep(&r), "got {r:?}");
    });
}

#[test]
fn far_deeper_than_max_is_refused_not_overflowed() {
    on_small_stack(|| {
        let (w, e) = world();
        let r = go(
            &w,
            &plan(&["x"], expand_chain(e, 50_000)),
            &Limits::default(),
        );
        assert!(is_too_deep(&r), "got {r:?}");
    });
}

#[test]
fn left_deep_join_nesting_at_the_limit_runs_and_one_over_is_too_deep() {
    on_small_stack(|| {
        let (w, e) = world();
        // k joins over 2-operator leaves: depth = k + 2.
        let build = |depth: usize| {
            let mut op = leaf(e);
            for _ in 0..depth - 2 {
                op = join(op, leaf(e));
            }
            op
        };
        let rs = go(&w, &plan(&["x"], build(MAX_PLAN_DEPTH)), &Limits::default()).unwrap();
        assert_eq!(rs.rows, vec![vec![Value::I64(7)]]);
        let r = go(
            &w,
            &plan(&["x"], build(MAX_PLAN_DEPTH + 1)),
            &Limits::default(),
        );
        assert!(is_too_deep(&r), "got {r:?}");
    });
}

#[test]
fn deep_union_nesting_at_the_limit_runs_and_one_over_is_too_deep() {
    on_small_stack(|| {
        let (w, e) = world();
        let build = |depth: usize| {
            let mut op = leaf(e);
            for _ in 0..depth - 2 {
                op = Op::Union {
                    inputs: vec![op, leaf(e)],
                };
            }
            op
        };
        let rs = go(&w, &plan(&["x"], build(MAX_PLAN_DEPTH)), &Limits::default()).unwrap();
        assert_eq!(rs.rows, vec![vec![Value::I64(7)]]);
        let r = go(
            &w,
            &plan(&["x"], build(MAX_PLAN_DEPTH + 1)),
            &Limits::default(),
        );
        assert!(is_too_deep(&r), "got {r:?}");
    });
}

#[test]
fn explain_of_a_plan_deeper_than_the_limit_works() {
    on_small_stack(|| {
        let (w, e) = world();
        let nodes = MAX_PLAN_DEPTH * 3;
        let p = plan(&["x"], expand_chain(e, nodes));
        let text = p.explain(&w);
        assert!(text.len() <= MAX_EXPLAIN_BYTES + 64, "{} bytes", text.len());
        assert!(text.starts_with("Find ?x"));
        assert!(text.ends_with("  ... (plan truncated)\n"));
    });
}

#[test]
fn dropping_a_100_000_deep_expand_chain_does_not_overflow() {
    on_small_stack(|| {
        let (_w, e) = world();
        let op = expand_chain(e, 100_000);
        drop(op);
    });
}

#[test]
fn union_mismatch_is_reported_before_a_later_inputs_compile_error() {
    on_small_stack(|| {
        let (w, e) = world();
        let u = Op::Union {
            inputs: vec![
                leaf(e),                                                  // binds x
                node_scan(Op::Unit, "y", "CHARACTER"),                    // binds y: mismatch
                expand(tr(e), "no_such_attr", tv("x"), at(0), Access::E), // compile error
            ],
        };
        let r = go(&w, &plan(&["x"], u), &Limits::default());
        match r {
            Err(QueryError::Plan(m)) => assert!(m.contains("Union"), "message: {m}"),
            other => panic!("expected the Union mismatch Plan error, got {other:?}"),
        }
    });
}

#[test]
fn row_limit_and_step_budget_inside_a_union_still_error() {
    on_small_stack(|| {
        let schema = Schema::new().node_type("CHARACTER").attribute(AttrDef::new(
            "n",
            ValueType::I64,
            Kind::State,
            Cardinality::One,
        ));
        let mut w = World::in_memory(schema).unwrap();
        for i in 0..20 {
            let e = w.create_entity("CHARACTER", None).unwrap();
            w.assert(e, "n", Value::I64(i), Some(Tick(0))).unwrap();
        }
        let scan = || node_scan(Op::Unit, "x", "CHARACTER");
        let p = plan(
            &["x"],
            Op::Union {
                inputs: vec![scan(), scan(), scan()],
            },
        );
        // Sanity: it runs under default limits and yields the 20 distinct rows.
        assert_eq!(go(&w, &p, &Limits::default()).unwrap().rows.len(), 20);
        let rows = Limits {
            max_rows: 10,
            ..Limits::default()
        };
        assert!(matches!(
            go(&w, &p, &rows),
            Err(QueryError::RowLimit { limit: 10 })
        ));
        let steps = Limits {
            max_steps: 5,
            ..Limits::default()
        };
        assert!(matches!(
            go(&w, &p, &steps),
            Err(QueryError::StepBudget { limit: 5 })
        ));
    });
}
