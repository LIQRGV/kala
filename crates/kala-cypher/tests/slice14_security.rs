//! Regressions from the slice-14 review: staged list values stay bounded.
use kala_core::Value;
use kala_fixture::load_fixture;
use kala_query::{Datum, Limits, QueryError, QueryParams, run};

fn world() -> kala_core::World {
    load_fixture(
        "schema:\n  node_types: [NODE]\n  attributes: []\nentities: {}\ntransactions: []\n",
    )
    .unwrap()
}

fn long_prefix_params() -> QueryParams {
    let first = (0..4096)
        .map(|_| Datum::from(Value::I64(1)))
        .collect::<Vec<_>>();
    let mut second = first.clone();
    second.push(Value::I64(2).into());
    let mut params = QueryParams::new();
    params.insert(
        "lists".into(),
        Datum::List(vec![Datum::List(first), Datum::List(second)]),
    );
    params.insert(
        "copies".into(),
        Datum::List((0..128).map(|i| Datum::from(Value::I64(i))).collect()),
    );
    params
}

#[test]
fn with_order_by_list_elements_charge_comparison_steps() {
    let world = world();
    let p = kala_cypher::prepare(
        &world,
        "UNWIND $copies AS n UNWIND $lists AS k WITH k ORDER BY k RETURN count(*)",
    )
    .unwrap();
    let result = run(
        &world,
        &p.plan,
        &long_prefix_params(),
        &Limits {
            max_steps: 20_000,
            ..Limits::default()
        },
    );
    assert!(
        matches!(result, Err(QueryError::StepBudget { .. })),
        "list prefixes require work beyond the budget: {result:?}"
    );
}

#[test]
fn min_max_list_elements_charge_comparison_steps() {
    let world = world();
    for function in ["min", "max"] {
        let p = kala_cypher::prepare(
            &world,
            &format!("UNWIND $copies AS n UNWIND $lists AS k RETURN {function}(k)"),
        )
        .unwrap();
        let result = run(
            &world,
            &p.plan,
            &long_prefix_params(),
            &Limits {
                max_steps: 20_000,
                ..Limits::default()
            },
        );
        assert!(
            matches!(result, Err(QueryError::StepBudget { .. })),
            "{function}: list prefixes require work beyond the budget; result rows/error: {:?}",
            result.as_ref().map(|rows| rows.rows.len())
        );
    }
}

#[test]
fn staged_list_nesting_is_bounded_even_when_final_value_is_small() {
    let world = world();
    let query = format!(
        "UNWIND [1] AS x {} RETURN size(x)",
        "WITH [x] AS x ".repeat(40)
    );
    let p = kala_cypher::prepare(&world, &query).unwrap();
    let result = run(&world, &p.plan, &QueryParams::new(), &Limits::default());
    assert!(
        result.is_err(),
        "each stage must preserve the list depth bound: {result:?}"
    );
}

#[test]
fn shared_list_doubling_is_bounded_before_result_materialization() {
    let world = world();
    let query = format!(
        "UNWIND [1] AS x {} RETURN size(x)",
        "WITH [x,x] AS x ".repeat(20)
    );
    let p = kala_cypher::prepare(&world, &query).unwrap();
    let result = run(
        &world,
        &p.plan,
        &QueryParams::new(),
        &Limits {
            max_bytes: 64 * 1024,
            ..Limits::default()
        },
    );
    assert!(
        matches!(result, Err(QueryError::MemoryBudget { .. })),
        "small shared representation must not bypass expanded-value limit: {result:?}"
    );
}
