//! Slice 14: bag aggregation, including Cypher's ungrouped empty-input row.
use kala_core::{Value, World};
use kala_fixture::load_fixture;
use kala_query::{Datum, Limits, Params, run};

const VARROW: &str = include_str!("../../kala-core/tests/fixtures/varrow.yaml");

fn query(world: &World, text: &str) -> Vec<Vec<Datum>> {
    let prepared = kala_cypher::prepare(world, text);
    assert!(
        prepared.is_ok(),
        "Cypher aggregate must compile: {prepared:?}"
    );
    run(
        world,
        &prepared.unwrap().plan,
        &Params::new(),
        &Limits::default(),
    )
    .unwrap_or_else(|error| panic!("{text}: {error}"))
    .rows
}

fn int(value: i64) -> Datum {
    Value::I64(value).into()
}

fn string(value: &str) -> Datum {
    Value::Str(value.into()).into()
}

#[test]
fn count_star_over_bag_counts_duplicates() {
    let world = load_fixture(VARROW).unwrap();
    let baseline =
        kala_datalog::prepare(&world, "[:find ?a ?b :at -208 :where [?a :ally_of ?b]]").unwrap();
    let pairs = run(&world, &baseline.plan, &Params::new(), &Limits::default()).unwrap();
    assert_eq!(pairs.rows.len(), 30, "fixture has 30 ordered ally pairs");
    assert_eq!(
        query(
            &world,
            "MATCH (a)-[:ally_of]->(b) AT TICK -208 RETURN count(*)"
        ),
        vec![vec![int(30)]]
    );
}

#[test]
fn zero_rows_without_grouping_give_one_row() {
    let world = load_fixture(VARROW).unwrap();
    assert_eq!(
        query(
            &world,
            "MATCH (c {kala_handle: 'nobody'}) RETURN count(*), collect(c.name), min(c.name), sum(1), avg(1)"
        ),
        vec![vec![
            int(0),
            Datum::List(vec![]),
            Datum::Null,
            int(0),
            Datum::Null
        ]]
    );
    assert!(
        query(
            &world,
            "MATCH (c {kala_handle: 'nobody'}) RETURN c.name, count(*)"
        )
        .is_empty()
    );
}

#[test]
fn count_distinct_and_grouping_see_bag_input() {
    let world = load_fixture(VARROW).unwrap();
    assert_eq!(
        query(
            &world,
            "MATCH (a)-[:ally_of]->(b) AT TICK -208 RETURN count(a), count(DISTINCT a)"
        ),
        vec![vec![int(30), int(28)]]
    );
    assert_eq!(
        query(
            &world,
            "MATCH (a)-[:ally_of]->(b) AT TICK -208 RETURN a.name, count(b) ORDER BY count(b) DESC, a.name LIMIT 1"
        ),
        vec![vec![string("Edwin"), int(3)]]
    );
    let cypher = query(
        &world,
        "MATCH (e {kala_handle: 'edwin'})-[:ally_of]->(a) AT TICK -208 RETURN count(DISTINCT a)",
    );
    let datalog = kala_datalog::prepare(
        &world,
        "[:find (count-distinct ?a) :at -208 :where [:edwin :ally_of ?a]]",
    )
    .unwrap();
    let datalog = run(&world, &datalog.plan, &Params::new(), &Limits::default()).unwrap();
    assert_eq!(cypher, vec![vec![int(3)]]);
    assert_eq!(cypher, datalog.rows);
}

#[test]
fn grouped_entity_remains_available_to_order_by_property() {
    let world = load_fixture(VARROW).unwrap();
    let edwin = Datum::from(Value::Ref(world.entity_by_handle("edwin").unwrap()));
    assert_eq!(
        query(
            &world,
            "MATCH (a)-[:ally_of]->(b) AT TICK -208 RETURN a, count(b) ORDER BY a.name LIMIT 1"
        ),
        vec![vec![edwin, int(3)]]
    );
    assert_eq!(
        query(
            &world,
            "MATCH (a)-[:ally_of]->(b) AT TICK -208 WITH a, count(b) AS n ORDER BY a.name LIMIT 1 RETURN a.name, n"
        ),
        vec![vec![string("Edwin"), int(3)]]
    );
}

#[test]
fn collect_keeps_order_duplicates_and_skips_nulls() {
    let world = load_fixture(VARROW).unwrap();
    assert_eq!(
        query(
            &world,
            "UNWIND [3, null, 1, 3, 2] AS x RETURN count(*), count(x), count(DISTINCT x), collect(x), collect(DISTINCT x)"
        ),
        vec![vec![
            int(5),
            int(4),
            int(3),
            Datum::List(vec![int(3), int(1), int(3), int(2)]),
            Datum::List(vec![int(3), int(1), int(2)])
        ]]
    );
    // Mara acquires "Voss" at -100, after this clause's -208 binding tick.
    assert_eq!(
        query(
            &world,
            "MATCH (e {kala_handle: 'edwin'})-[:ally_of]->(a) AT TICK -208 WITH a ORDER BY a.name RETURN count(a), collect(a.name)"
        ),
        vec![vec![
            int(3),
            Datum::List(vec![string("Greg"), string("Mara"), string("Tomas")])
        ]]
    );
}

#[test]
fn numeric_aggregates_skip_null_and_average_is_float() {
    let world = load_fixture(VARROW).unwrap();
    assert_eq!(
        query(
            &world,
            "UNWIND [1, null, 3, 2] AS x RETURN min(x), max(x), sum(x), avg(x)"
        ),
        vec![vec![int(1), int(3), int(6), Value::F64(2.0).into()]]
    );
    assert_eq!(
        query(
            &world,
            "UNWIND [null] AS x RETURN count(*), count(x), min(x), max(x), sum(x), avg(x), collect(x)"
        ),
        vec![vec![
            int(1),
            int(0),
            Datum::Null,
            Datum::Null,
            int(0),
            Datum::Null,
            Datum::List(vec![])
        ]]
    );
    assert_eq!(
        query(&world, "UNWIND [1.5, 2.5] AS x RETURN sum(x), avg(x)"),
        vec![vec![
            Datum::from(Value::F64(4.0)),
            Datum::from(Value::F64(2.0))
        ]]
    );
    assert_eq!(
        query(&world, "UNWIND ['z', null, 'a'] AS x RETURN min(x), max(x)"),
        vec![vec![string("a"), string("z")]]
    );
}

#[test]
fn sum_integer_overflow_and_invalid_types_are_errors() {
    let world = load_fixture(VARROW).unwrap();
    for text in [
        "UNWIND [9223372036854775807, 1] AS x RETURN sum(x)",
        "UNWIND [-9223372036854775808, -1] AS x RETURN sum(x)",
        "UNWIND ['abc'] AS x RETURN sum(x)",
        "UNWIND [true] AS x RETURN avg(x)",
    ] {
        let prepared = kala_cypher::prepare(&world, text).unwrap();
        let result = run(&world, &prepared.plan, &Params::new(), &Limits::default());
        assert!(result.is_err(), "{text}: {result:?}");
    }
}

#[test]
fn aggregates_at_edge_ticks_and_after_retractions_match_datalog() {
    let world = load_fixture(VARROW).unwrap();
    for tick in [i64::MIN, -301, -300, -210, -208, -200, 0, 700, i64::MAX] {
        let datalog = kala_datalog::prepare(
            &world,
            &format!("[:find ?a :at {tick} :where [:edwin :ally_of ?a]]"),
        )
        .unwrap();
        let expected = run(&world, &datalog.plan, &Params::new(), &Limits::default())
            .unwrap()
            .rows
            .len();
        assert_eq!(
            query(
                &world,
                &format!(
                    "MATCH (e {{kala_handle: 'edwin'}})-[:ally_of]->(a) AT TICK {tick} RETURN count(*)"
                )
            ),
            vec![vec![int(expected as i64)]],
            "tick {tick}"
        );
    }
}

#[test]
fn aggregate_errors_reject_nesting_and_where_positions() {
    let world = load_fixture(VARROW).unwrap();
    for text in [
        "MATCH (a) RETURN count(collect(a))",
        "MATCH (a) RETURN sum(count(a))",
        "MATCH (a) WHERE count(a) > 1 RETURN a",
        "MATCH (a) WITH a WHERE count(a) > 1 RETURN a",
        "MATCH (a) RETURN collect(*)",
    ] {
        assert!(kala_cypher::prepare(&world, text).is_err(), "{text}");
    }
}

#[test]
fn collect_is_charged_to_the_memory_budget() {
    let world = load_fixture(VARROW).unwrap();
    let value = "x".repeat(4096);
    let limits = Limits {
        max_bytes: 64 * 1024,
        ..Limits::default()
    };
    let count = kala_cypher::prepare(
        &world,
        &format!("MATCH (n:CHARACTER) RETURN count('{value}')"),
    )
    .unwrap();
    assert_eq!(
        run(&world, &count.plan, &Params::new(), &limits)
            .unwrap()
            .rows,
        vec![vec![int(33)]]
    );
    let prepared = kala_cypher::prepare(
        &world,
        &format!("MATCH (n:CHARACTER) RETURN collect('{value}')"),
    )
    .unwrap();
    let result = run(&world, &prepared.plan, &Params::new(), &limits);
    assert!(
        matches!(result, Err(kala_query::QueryError::MemoryBudget { .. })),
        "{result:?}"
    );
}
