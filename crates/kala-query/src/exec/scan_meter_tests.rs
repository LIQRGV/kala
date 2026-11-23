use super::*;
use crate::ir::{Access, AttrRef, Op, Term};
use kala_core::{AttrDef, Cardinality, Kind, Schema, ValueType, WorkInterruptReason};
use proptest::prelude::*;
use std::sync::atomic::Ordering as AtomicOrdering;

/// [`Request::execute_inner`] with the request spelled out, so a test can
/// run a plan under a meter of its own.
fn execute_inner(
    world: &World,
    view: &View,
    plan: &Plan,
    params: &Params,
    limits: &Limits,
    cancel: &AtomicBool,
    meter: &WorkMeter,
) -> Result<(ResultSet, Usage), QueryError> {
    let request = Request {
        world,
        plan,
        params,
        limits,
        cancel,
    };
    request.execute_inner(view, meter)
}

fn world(values: &[i64]) -> World {
    let mut world = World::in_memory(Schema::new().node_type("N").attribute(AttrDef::new(
        "score",
        ValueType::I64,
        Kind::State,
        Cardinality::Many,
    )))
    .unwrap();
    let mut tx = world.begin();
    for &value in values {
        let entity = tx.create_entity("N", None).unwrap();
        tx.assert(entity, "score", Value::I64(value), Some(Tick(-1)))
            .unwrap();
    }
    tx.commit().unwrap();
    world
}

fn scan_plan() -> Plan {
    Plan {
        find: vec![Var("e".into()), Var("v".into())],
        params: Vec::new(),
        default_tick: TickSource::Const(Tick(0)),
        canon: CanonSource::Latest,
        root: Op::Expand {
            input: Box::new(Op::Unit),
            entity: Term::Var(Var("e".into())),
            attr: AttrRef::Named("score".into()),
            value: Term::Var(Var("v".into())),
            tick: TickSource::Default,
            access: Access::Scan,
        },
    }
}

#[test]
fn one_scan_is_interrupted_before_finishing_its_key_walk() {
    let world = world(&(0..10_000).collect::<Vec<_>>());
    let limits = Limits {
        max_steps: 100,
        ..Limits::default()
    };
    let cancel = AtomicBool::new(false);
    let meter = WorkMeter::bounded(limits.max_steps, Some(&cancel));
    let result = execute_inner(
        &world,
        &world.at_canon(world.latest_tx()),
        &scan_plan(),
        &Params::new(),
        &limits,
        &cancel,
        &meter,
    );
    assert!(
        matches!(
            result,
            Err(QueryError::Core(CoreError::WorkInterrupted {
                reason: WorkInterruptReason::Limit
            }))
        ),
        "scan must stop inside kala-core; got {result:?}"
    );
    assert!(
        (1..=101).contains(&meter.keys()),
        "scan examined {} keys",
        meter.keys()
    );
}

#[test]
fn cancellation_interrupts_one_scan_before_all_keys_are_read() {
    let world = world(&(0..100_000).collect::<Vec<_>>());
    let cancel = AtomicBool::new(false);
    let meter = WorkMeter::bounded(u64::MAX, Some(&cancel));
    std::thread::scope(|scope| {
        let reader = scope.spawn(|| {
            execute_inner(
                &world,
                &world.at_canon(world.latest_tx()),
                &scan_plan(),
                &Params::new(),
                &Limits::default(),
                &cancel,
                &meter,
            )
        });
        while meter.keys() < 100 && !reader.is_finished() {
            std::thread::yield_now();
        }
        cancel.store(true, AtomicOrdering::Relaxed);
        let result = reader.join().unwrap();
        assert!(
            matches!(
                result,
                Err(QueryError::Core(CoreError::WorkInterrupted {
                    reason: WorkInterruptReason::Stopped
                }))
            ),
            "cancellation must land in the core scan, got {result:?}"
        );
        assert!(
            (100..100_000).contains(&meter.keys()),
            "scan completed {} keys",
            meter.keys()
        );
    });
}

#[test]
fn repeated_empty_scans_share_the_query_id_budget() {
    // Every scan crosses 5,001 IDs, but nearly all prefix seeks miss:
    // only the first ID has facts, under a different attribute. Ten
    // per-row ticks must not each receive a fresh 10,000-ID allowance.
    let schema = Schema::new()
        .node_type("N")
        .attribute(AttrDef::new(
            "score",
            ValueType::I64,
            Kind::State,
            Cardinality::Many,
        ))
        .attribute(AttrDef::new(
            "tick",
            ValueType::I64,
            Kind::State,
            Cardinality::Many,
        ));
    let mut world = World::in_memory(schema).unwrap();
    let mut tx = world.begin();
    let seed = tx.create_entity("N", None).unwrap();
    for tick in 0..10 {
        tx.assert(seed, "tick", Value::I64(tick), Some(Tick(0)))
            .unwrap();
    }
    for _ in 0..5_000 {
        tx.create_entity("N", None).unwrap();
    }
    tx.commit().unwrap();
    let tick_var = Var("t".into());
    let mut plan = scan_plan();
    if let Op::Expand { input, tick, .. } = &mut plan.root {
        **input = Op::Expand {
            input: Box::new(Op::Unit),
            entity: Term::Const(Value::Ref(seed)),
            attr: AttrRef::Named("tick".into()),
            value: Term::Var(tick_var.clone()),
            tick: TickSource::Const(Tick(0)),
            access: Access::E,
        };
        *tick = TickSource::Var(tick_var);
    }
    let limits = Limits {
        max_steps: 10_000,
        ..Limits::default()
    };
    let cancel = AtomicBool::new(false);
    let meter = WorkMeter::bounded(limits.max_steps, Some(&cancel));
    let result = execute_inner(
        &world,
        &world.at_canon(world.latest_tx()),
        &plan,
        &Params::new(),
        &limits,
        &cancel,
        &meter,
    );
    assert!(
        matches!(
            result,
            Err(QueryError::Core(CoreError::WorkInterrupted {
                reason: WorkInterruptReason::Limit
            })) | Err(QueryError::StepBudget { .. })
        ),
        "repeated empty scans escaped the query ID budget: {result:?}"
    );
}

proptest! {
    #[test]
    fn bounded_and_unbounded_execution_have_identical_rows_and_steps(
        values in prop::collection::vec(-10i64..11, 0..40),
        ticks in prop::sample::select(vec![i64::MIN, -2, -1, 0, i64::MAX]),
        main_index in any::<bool>(),
        default_tick in any::<bool>(),
    ) {
        let mut world = world(&values);
        let ids: Vec<_> = world.entities_of_type("N").unwrap().collect();
        let mut tx = world.begin();
        for (&id, &value) in ids.iter().zip(&values) {
            if value % 2 == 0 {
                tx.retract(id, "score", Value::I64(value), Some(Tick(1))).unwrap();
            }
            if value < 0 {
                tx.assert(id, "score", Value::I64(value), Some(Tick(i64::MAX))).unwrap();
            }
        }
        tx.commit().unwrap();
        let canon = world.latest_tx();
        if main_index { world.create_entity("N", None).unwrap(); }
        let view = world.at_canon(canon);
        let mut plan = scan_plan();
        plan.default_tick = if default_tick { TickSource::Default } else { TickSource::Const(Tick(ticks)) };
        let cancel = AtomicBool::new(false);
        let limits = Limits::default();
        let unbounded = WorkMeter::new();
        let bounded = WorkMeter::bounded(limits.max_steps, Some(&cancel));
        let (want, want_usage) = execute_inner(&world, &view, &plan, &Params::new(), &limits, &cancel, &unbounded).unwrap();
        let (got, got_usage) = execute_inner(&world, &view, &plan, &Params::new(), &limits, &cancel, &bounded).unwrap();
        prop_assert_eq!(got, want);
        prop_assert_eq!(got_usage.steps, want_usage.steps);
    }
}
