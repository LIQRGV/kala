//! Cancellation is per execution, independent of resource limits.
mod common;

use common::*;
use kala_core::{Schema, World};
use kala_query::{Limits, Op, Params, QueryError, run, run_with_cancel};
use proptest::prelude::*;
use std::sync::atomic::{AtomicBool, Ordering};

#[test]
fn a_pre_set_flag_cancels_before_execution() {
    let w = World::in_memory(Schema::new()).unwrap();
    let p = plan(&[], Op::Unit);
    let flag = AtomicBool::new(true);
    let result = run_with_cancel(&w, &p, &Params::new(), &Limits::default(), &flag);
    assert!(matches!(result, Err(QueryError::Cancelled)), "{result:?}");
}

#[test]
fn an_unset_flag_preserves_results_and_can_be_reused_after_cancel() {
    let w = World::in_memory(Schema::new()).unwrap();
    let p = plan(&[], Op::Unit);
    let flag = AtomicBool::new(true);
    assert!(matches!(
        run_with_cancel(&w, &p, &Params::new(), &Limits::default(), &flag),
        Err(QueryError::Cancelled)
    ));
    flag.store(false, Ordering::Relaxed);
    let expected = run(&w, &p, &Params::new(), &Limits::default()).unwrap();
    let actual = run_with_cancel(&w, &p, &Params::new(), &Limits::default(), &flag).unwrap();
    assert_eq!(actual.rows, expected.rows);
    assert_eq!(actual.columns, expected.columns);
    assert_eq!(actual.tick, expected.tick);
    assert_eq!(actual.canon, expected.canon);
}

#[test]
fn another_thread_can_cancel_a_query_that_would_exhaust_steps() {
    let mut world = World::in_memory(Schema::new().node_type("N")).unwrap();
    for _ in 0..200 {
        world.create_entity("N", None).unwrap();
    }
    let root = node_scan(node_scan(node_scan(Op::Unit, "a", "N"), "b", "N"), "c", "N");
    let p = plan(&["a", "b", "c"], project(root, &["a", "b", "c"]));
    let limits = Limits {
        max_rows: 1_000_000,
        max_steps: 100_000,
        max_bytes: 512 * 1024 * 1024,
    };
    assert!(matches!(
        run(&world, &p, &Params::new(), &limits),
        Err(QueryError::StepBudget { .. })
    ));
    let flag = AtomicBool::new(false);
    let start = std::sync::Barrier::new(2);
    std::thread::scope(|scope| {
        scope.spawn(|| {
            start.wait();
            // Encourage overlap without tying correctness to elapsed time.
            for _ in 0..100 {
                std::thread::yield_now();
            }
            flag.store(true, Ordering::Release);
        });
        start.wait();
        let result = run_with_cancel(&world, &p, &Params::new(), &limits, &flag);
        assert!(matches!(result, Err(QueryError::Cancelled)), "{result:?}");
    });
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]
    #[test]
    fn unset_flag_matches_run_for_random_scan_plans(nodes in 0usize..24, cross in any::<bool>()) {
        let mut world = World::in_memory(Schema::new().node_type("N")).unwrap();
        for _ in 0..nodes { world.create_entity("N", None).unwrap(); }
        let root = node_scan(Op::Unit, "a", "N");
        let root = if cross { node_scan(root, "b", "N") } else { root };
        let p = plan(&["a"], project(root, &["a"]));
        let expected = run(&world, &p, &Params::new(), &Limits::default()).unwrap();
        let actual = run_with_cancel(&world, &p, &Params::new(), &Limits::default(), &AtomicBool::new(false)).unwrap();
        prop_assert_eq!(actual.rows, expected.rows);
        prop_assert_eq!(actual.columns, expected.columns);
    }
}
