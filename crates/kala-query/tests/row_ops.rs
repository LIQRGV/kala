//! The neutral row IR independently of any language compiler.
use kala_core::{Value, load_fixture};
use kala_query::{
    CanonSource, Datum, Expr, Limits, Op, OutputSpec, Params, Plan, QueryError, RowAction,
    RunOptions, Term, TickSource, Var, run, run_with,
};
use std::sync::atomic::AtomicBool;
fn var(name: &str) -> Var {
    Var(name.into())
}
fn bind(input: Op, name: &str, expr: Expr) -> Op {
    Op::Row {
        input: Box::new(input),
        action: RowAction::Bind {
            var: var(name),
            expr,
        },
    }
}
fn input(values: &[Option<Value>]) -> Op {
    Op::Union {
        inputs: values
            .iter()
            .enumerate()
            .map(|(i, v)| {
                bind(
                    bind(Op::Unit, "identity", Expr::Value(Value::I64(i as i64))),
                    "value",
                    v.clone().map_or(Expr::Null, Expr::Value),
                )
            })
            .collect(),
    }
}
fn plan(
    values: &[Option<Value>],
    distinct: bool,
    order: Vec<(Var, bool)>,
    skip: Option<Term>,
    limit: Option<Term>,
) -> Plan {
    Plan {
        find: vec![var("value")],
        params: vec![],
        default_tick: TickSource::Default,
        canon: CanonSource::Latest,
        root: Op::Output {
            input: Box::new(input(values)),
            spec: OutputSpec {
                vars: vec![var("value")],
                distinct,
                order,
                skip,
                limit,
            },
        },
    }
}
fn world() -> kala_core::World {
    load_fixture(
        "schema:\n  node_types: [NODE]\n  attributes: []\nentities: {}\ntransactions: []\n",
    )
    .unwrap()
}

#[test]
fn neutral_output_keeps_bags_then_distinct_then_order_then_slice() {
    let w = world();
    let values = [
        Some(Value::I64(3)),
        Some(Value::I64(1)),
        Some(Value::I64(3)),
        None,
    ];
    let sorted = vec![(var("value"), false)];
    let bag = run(
        &w,
        &plan(&values, false, sorted.clone(), None, None),
        &Params::new(),
        &Limits::default(),
    )
    .unwrap();
    assert_eq!(
        bag.rows,
        vec![
            vec![Datum::from(Value::I64(1))],
            vec![Datum::from(Value::I64(3))],
            vec![Datum::from(Value::I64(3))],
            vec![Datum::Null]
        ]
    );
    let distinct = run(
        &w,
        &plan(
            &values,
            true,
            sorted,
            Some(Term::Const(Value::I64(1))),
            Some(Term::Const(Value::I64(2))),
        ),
        &Params::new(),
        &Limits::default(),
    )
    .unwrap();
    assert_eq!(
        distinct.rows,
        vec![vec![Datum::from(Value::I64(3))], vec![Datum::Null]]
    );
    let descending = run(
        &w,
        &plan(
            &values,
            true,
            vec![(var("value"), true)],
            None,
            Some(Term::Const(Value::I64(2))),
        ),
        &Params::new(),
        &Limits::default(),
    )
    .unwrap();
    assert_eq!(
        descending.rows,
        vec![vec![Datum::Null], vec![Datum::from(Value::I64(3))]]
    );
}

#[test]
fn orderability_uses_numeric_comparison_without_rounding_large_integers() {
    let w = world();
    let expected = vec![
        Some(Value::Str("A".into())),
        Some(Value::Str("z".into())),
        Some(Value::Bool(false)),
        Some(Value::Bool(true)),
        Some(Value::I64(i64::MIN)),
        Some(Value::F64(-1.5)),
        Some(Value::I64(0)),
        Some(Value::F64(0.5)),
        Some(Value::F64(9007199254740992.0)),
        Some(Value::I64(9007199254740993)),
        Some(Value::I64(i64::MAX)),
        Some(Value::F64(9223372036854775808.0)),
        None,
    ];
    let mut shuffled = expected.clone();
    shuffled.reverse();
    let p = plan(&shuffled, false, vec![(var("value"), false)], None, None);
    let result = run(&w, &p, &Params::new(), &Limits::default()).unwrap();
    let rows: Vec<Vec<Datum>> = expected
        .into_iter()
        .map(|v| vec![v.map_or(Datum::Null, Datum::from)])
        .collect();
    assert_eq!(result.rows, rows);
}

#[test]
fn sorting_charges_extra_work_and_memory_and_checks_cancellation() {
    let w = world();
    let values: Vec<_> = (0..512)
        .rev()
        .map(|n| Some(Value::Str(format!("{n:04}{}", "x".repeat(128)))))
        .collect();
    let plain = plan(&values, false, vec![], None, None);
    let ordered = plan(
        &values,
        false,
        vec![(var("value"), false)],
        None,
        Some(Term::Const(Value::I64(1))),
    );
    let (_, plain_usage) = run_with(
        &w,
        &plain,
        &Params::new(),
        RunOptions::new(Limits::default()),
    )
    .unwrap();
    let (result, ordered_usage) = run_with(
        &w,
        &ordered,
        &Params::new(),
        RunOptions::new(Limits::default()),
    )
    .unwrap();
    assert_eq!(result.rows.len(), 1);
    assert!(
        ordered_usage.steps > plain_usage.steps,
        "sort must be metered: {ordered_usage:?} / {plain_usage:?}"
    );
    assert!(matches!(
        run(
            &w,
            &ordered,
            &Params::new(),
            &Limits {
                max_steps: plain_usage.steps,
                ..Limits::default()
            }
        ),
        Err(QueryError::StepBudget { .. })
    ));
    assert!(matches!(
        run(
            &w,
            &ordered,
            &Params::new(),
            &Limits {
                max_bytes: ordered_usage.peak_bytes - 1,
                ..Limits::default()
            }
        ),
        Err(QueryError::MemoryBudget { .. })
    ));
    assert!(matches!(
        run(
            &w,
            &ordered,
            &Params::new(),
            &Limits {
                max_rows: 511,
                ..Limits::default()
            }
        ),
        Err(QueryError::RowLimit { .. })
    ));
    assert!(matches!(
        run_with(
            &w,
            &ordered,
            &Params::new(),
            RunOptions::new(Limits::default()).with_cancel(&AtomicBool::new(true))
        )
        .map(|(rs, _)| rs),
        Err(QueryError::Cancelled)
    ));
}

#[test]
fn nullable_expression_attributes_are_visible_before_evaluation() {
    let property = |key: &str| Expr::Property {
        entity: var("n"),
        key: key.into(),
        tick: TickSource::Default,
    };
    let root = Op::Row {
        input: Box::new(bind(Op::Unit, "sort-key", property("password_hash"))),
        action: RowAction::Select(Expr::Or(
            Box::new(Expr::Value(Value::Bool(true))),
            Box::new(Expr::IsNull(Box::new(property("secret_in_false_branch")))),
        )),
    };
    let p = Plan {
        find: vec![],
        params: vec![],
        default_tick: TickSource::Default,
        canon: CanonSource::Latest,
        root,
    };
    let names = p.attribute_names();
    assert!(names.contains(&"password_hash"));
    assert!(names.contains(&"secret_in_false_branch"));
}

#[test]
fn distinct_output_accounts_for_hash_keys_while_projected_rows_are_live() {
    let w = world();
    let values: Vec<_> = (0..512).map(|n| Some(Value::I64(n))).collect();
    let bag = plan(&values, false, vec![], None, None);
    let distinct = plan(&values, true, vec![], None, None);
    let (bag_rows, bag_usage) =
        run_with(&w, &bag, &Params::new(), RunOptions::new(Limits::default())).unwrap();
    let (distinct_rows, distinct_usage) = run_with(
        &w,
        &distinct,
        &Params::new(),
        RunOptions::new(Limits::default()),
    )
    .unwrap();
    assert_eq!(distinct_rows.rows, bag_rows.rows);
    assert!(
        distinct_usage.peak_bytes > bag_usage.peak_bytes,
        "DISTINCT keeps independent hash keys alongside output tuples: distinct {distinct_usage:?}, bag {bag_usage:?}"
    );
    let limited = Limits {
        max_bytes: bag_usage.peak_bytes,
        ..Limits::default()
    };
    assert!(run(&w, &bag, &Params::new(), &limited).is_ok());
    assert!(matches!(
        run(&w, &distinct, &Params::new(), &limited),
        Err(QueryError::MemoryBudget { .. })
    ));
}
