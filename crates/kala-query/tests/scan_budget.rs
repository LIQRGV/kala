//! Resource interruptions must surface as query limits, never store errors.
mod common;
use common::*;
use kala_core::{AttrDef, Cardinality, Kind, Schema, Tick, Value, ValueType, World};
use kala_query::{Access, Limits, Op, OpTerm, Params, QueryError, run, run_with_cancel};
use std::sync::atomic::AtomicBool;

fn world() -> World {
    let mut w = World::in_memory(Schema::new().node_type("N").attribute(AttrDef::new(
        "score",
        ValueType::I64,
        Kind::State,
        Cardinality::Many,
    )))
    .unwrap();
    let mut tx = w.begin();
    for i in 0..10_000 {
        let e = tx.create_entity("N", None).unwrap();
        tx.assert(e, "score", Value::I64(i), Some(Tick(0))).unwrap();
    }
    tx.commit().unwrap();
    w
}

#[test]
fn whole_attribute_and_history_scans_report_step_budget() {
    let w = world();
    for root in [
        expand(tv("e"), "score", tv("s"), at(0), Access::Scan),
        Op::History {
            input: b(Op::Unit),
            entity: tv("e"),
            attr: "score".into(),
            value: tv("s"),
            tick: tv("t"),
            op: OpTerm::Any,
            precision: kala_query::Term::Any,
        },
    ] {
        let p = plan(&["e", "s"], root);
        let result = run(
            &w,
            &p,
            &Params::new(),
            &Limits {
                max_steps: 100,
                ..Limits::default()
            },
        );
        assert!(
            matches!(result, Err(QueryError::StepBudget { limit: 100 })),
            "wrong scan error: {result:?}"
        );
    }
}

#[test]
fn a_set_cancel_flag_reports_cancelled() {
    let w = world();
    let p = plan(
        &["e", "s"],
        expand(tv("e"), "score", tv("s"), at(0), Access::Scan),
    );
    let result = run_with_cancel(
        &w,
        &p,
        &Params::new(),
        &Limits::default(),
        &AtomicBool::new(true),
    );
    assert!(
        matches!(result, Err(QueryError::Cancelled)),
        "wrong cancellation error: {result:?}"
    );
}
