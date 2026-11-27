//! OPTIONAL MATCH, WITH scopes and UNWIND share the ordinary row pipeline.
use kala_core::{Value, World};
use kala_fixture::load_fixture;
use kala_query::{Datum, Limits, Params, QueryParams, run};

const VARROW: &str = include_str!("../../kala-core/tests/fixtures/varrow.yaml");
fn query(world: &World, text: &str) -> Vec<Vec<Datum>> {
    let p = kala_cypher::prepare(world, text).unwrap_or_else(|e| panic!("{text}: {e}"));
    run(world, &p.plan, &Params::new(), &Limits::default())
        .unwrap_or_else(|e| panic!("{text}: {e}"))
        .rows
}
fn string(value: &str) -> Datum {
    Value::Str(value.into()).into()
}
fn int(value: i64) -> Datum {
    Value::I64(value).into()
}

#[test]
fn optional_keeps_unmatched_left_and_null_properties() {
    let world = load_fixture(VARROW).unwrap();
    assert_eq!(
        query(
            &world,
            "MATCH (c:CHARACTER {kala_handle: 'kael'}) OPTIONAL MATCH (c)-[:ally_of]->(a) RETURN c.name, a, a.name"
        ),
        vec![vec![string("Edwin"), Datum::Null, Datum::Null]]
    );
    assert_eq!(
        query(
            &world,
            "OPTIONAL MATCH (c {kala_handle: 'nobody'}) RETURN c, c.name"
        ),
        vec![vec![Datum::Null, Datum::Null]]
    );
    assert_eq!(
        query(
            &world,
            "OPTIONAL MATCH (c {kala_handle: 'nobody'}) OPTIONAL MATCH (c)-[:ally_of]->(a) RETURN c, a"
        ),
        vec![vec![Datum::Null, Datum::Null]]
    );
    assert_eq!(
        query(
            &world,
            "OPTIONAL MATCH (c {kala_handle: 'nobody'}) WITH c RETURN count(*), count(c), collect(c)"
        ),
        vec![vec![int(1), int(0), Datum::List(vec![])]]
    );
    assert!(
        query(
            &world,
            "OPTIONAL MATCH (c {kala_handle: 'nobody'}) WITH c.alias AS xs UNWIND xs AS x RETURN x"
        )
        .is_empty()
    );
}

#[test]
fn optional_where_filters_match_and_keeps_left_bindings() {
    let world = load_fixture(VARROW).unwrap();
    assert_eq!(
        query(
            &world,
            "MATCH (c {kala_handle: 'edwin'}) OPTIONAL MATCH (c)-[:ally_of]->(a) AT TICK -208 WHERE a.name = 'Nobody' RETURN c.kala_handle, a"
        ),
        vec![vec![string("edwin"), Datum::Null]]
    );
    let rows = query(
        &world,
        "MATCH (c:CHARACTER) OPTIONAL MATCH (c)-[:ally_of]->(a) AT TICK -208 RETURN c.kala_handle, count(a) ORDER BY c.kala_handle",
    );
    assert_eq!(rows.len(), 33);
    assert!(rows.contains(&vec![string("edwin"), int(3)]));
    assert!(rows.contains(&vec![string("kael"), int(0)]));
    assert_eq!(rows.iter().filter(|row| row[1] == int(0)).count(), 5);
}

#[test]
fn with_projects_groups_filters_and_restricts_scope() {
    let world = load_fixture(VARROW).unwrap();
    assert_eq!(
        query(
            &world,
            "MATCH (a)-[:ally_of]->(b) AT TICK -208 WITH a, count(b) AS n WHERE n > 1 RETURN a.name, n"
        ),
        vec![vec![string("Edwin"), int(3)]]
    );
    assert_eq!(
        query(
            &world,
            "MATCH (c {kala_handle: 'edwin'}) AT TICK -208 WITH c AS e MATCH (a {kala_handle: 'greg'}) AT TICK 700 RETURN e.name, a.name"
        ),
        vec![vec![string("Edwin"), string("Greg")]]
    );
    assert_eq!(
        query(
            &world,
            "MATCH (c {kala_handle: 'edwin'}) AT TICK -208 WITH c.name AS old MATCH (c {kala_handle: 'edwin'}) AT TICK 700 RETURN old, c.name"
        ),
        vec![vec![string("Edwin"), string("the Grey King")]]
    );
    let err = kala_cypher::prepare(&world, "MATCH (a)-[:ally_of]->(b) WITH a RETURN b")
        .unwrap_err()
        .to_string();
    assert!(
        err.contains('b') && (err.contains("not defined") || err.contains("unbound")),
        "{err}"
    );
    assert!(kala_cypher::prepare(&world, "MATCH (a) WITH a.name RETURN a").is_err());
}

#[test]
fn with_graph_alias_can_order_by_its_property_after_grouping() {
    let world = load_fixture(VARROW).unwrap();
    assert_eq!(
        query(
            &world,
            "MATCH (a)-[:ally_of]->(b) AT TICK -208 WITH a AS x, count(b) AS n ORDER BY x.name LIMIT 1 RETURN x.kala_handle, n"
        ),
        vec![vec![string("edwin"), int(3)]]
    );
}

#[test]
fn a_handle_constraint_cannot_rebind_a_failed_optional_null() {
    let world = load_fixture(VARROW).unwrap();
    for text in [
        "OPTIONAL MATCH (c {kala_handle: 'nobody'}) MATCH (c {kala_handle: 'edwin'}) RETURN c",
        "OPTIONAL MATCH (c {kala_handle: 'nobody'}) WITH c AS x MATCH (x {kala_handle: 'edwin'}) RETURN x",
        "OPTIONAL MATCH (c {kala_handle: 'nobody'}) MATCH (c {kala_handle: 'edwin'})-[:ally_of]->(a) AT TICK -208 RETURN a",
    ] {
        assert!(query(&world, text).is_empty(), "{text}");
    }
}

#[test]
fn typed_unwind_parameter_origins_follow_with_scope() {
    let world = load_fixture(VARROW).unwrap();
    let mut params = QueryParams::new();
    params.insert(
        "names".into(),
        Datum::List(vec![string("Greg"), string("Hild")]),
    );
    let text = "UNWIND $names AS n WITH n AS name MATCH (c:CHARACTER {name: name}) RETURN c.kala_handle ORDER BY c.kala_handle";
    let p = kala_cypher::prepare(&world, text).unwrap();
    assert_eq!(
        run(&world, &p.plan, &params, &Limits::default())
            .unwrap()
            .rows,
        vec![vec![string("greg")], vec![string("hild")]]
    );
    params.insert("names".into(), Datum::List(vec![string("Greg"), int(3)]));
    for text in [
        text,
        "UNWIND $names AS n WITH n AS name WHERE false MATCH (c:CHARACTER {name: name}) RETURN c",
        "MATCH (z {kala_handle: 'nobody'}) UNWIND $names AS n WITH n AS name MATCH (c:CHARACTER {name: name}) RETURN c",
    ] {
        let p = kala_cypher::prepare(&world, text).unwrap();
        assert!(
            run(&world, &p.plan, &params, &Limits::default()).is_err(),
            "{text}"
        );
    }
    // A dropped binding's parameter type must not attach to a new variable
    // that happens to reuse its name in the next scope.
    params.insert("names".into(), Datum::List(vec![int(1), int(2)]));
    let p = kala_cypher::prepare(&world, "UNWIND $names AS n WITH 1 AS k UNWIND ['Greg'] AS n MATCH (c:CHARACTER {name: n}) RETURN c.kala_handle").unwrap();
    assert_eq!(
        run(&world, &p.plan, &params, &Limits::default())
            .unwrap()
            .rows,
        vec![vec![string("greg")]; 2]
    );
}

#[test]
fn with_star_distinct_order_skip_limit_and_shadowing() {
    let world = load_fixture(VARROW).unwrap();
    assert_eq!(
        query(
            &world,
            "UNWIND [3, 1, 3, 2] AS x WITH DISTINCT x ORDER BY x DESC SKIP 1 LIMIT 1 RETURN x"
        ),
        vec![vec![int(2)]]
    );
    assert_eq!(
        query(
            &world,
            "UNWIND [1, 2] AS x WITH *, x AS y RETURN x, y ORDER BY x"
        ),
        vec![vec![int(1), int(1)], vec![int(2), int(2)]]
    );
    assert_eq!(
        query(
            &world,
            "MATCH (c {kala_handle: 'edwin'}) WITH c.name AS c, c.kala_handle AS h RETURN c, h"
        ),
        vec![vec![string("the Grey King"), string("edwin")]]
    );
    assert_eq!(
        query(
            &world,
            "UNWIND [1, 2] AS x WITH x AS y WITH y AS x RETURN x ORDER BY x"
        ),
        vec![vec![int(1)], vec![int(2)]]
    );
}

#[test]
fn unwind_parameter_matches_fixture_names() {
    let world = load_fixture(VARROW).unwrap();
    let p = kala_cypher::prepare(&world, "UNWIND $names AS n MATCH (c:CHARACTER {name: n}) RETURN c.kala_handle ORDER BY c.kala_handle").unwrap();
    let mut params = QueryParams::new();
    params.insert(
        "names".into(),
        Datum::List(vec![string("Greg"), string("Hild"), string("Nobody")]),
    );
    assert_eq!(
        run(&world, &p.plan, &params, &Limits::default())
            .unwrap()
            .rows,
        vec![vec![string("greg")], vec![string("hild")]]
    );
    params.insert("names".into(), Datum::List(vec![string("Greg"), int(3)]));
    assert!(run(&world, &p.plan, &params, &Limits::default()).is_err());
    // Even a preceding empty clause must not hide a wrongly typed list element.
    let p = kala_cypher::prepare(&world, "MATCH (z {kala_handle: 'nobody'}) UNWIND $names AS n MATCH (c:CHARACTER {name: n}) RETURN c").unwrap();
    assert!(run(&world, &p.plan, &params, &Limits::default()).is_err());
}

#[test]
fn unwind_null_empty_and_nested_lists_preserve_order() {
    let world = load_fixture(VARROW).unwrap();
    for text in ["UNWIND [] AS x RETURN x", "UNWIND null AS x RETURN x"] {
        assert!(query(&world, text).is_empty());
    }
    assert_eq!(
        query(
            &world,
            "UNWIND [[2, 1], [3], []] AS xs UNWIND xs AS x RETURN x"
        ),
        vec![vec![int(2)], vec![int(1)], vec![int(3)]]
    );
    assert_eq!(
        query(&world, "UNWIND [null, 1, null] AS x RETURN x"),
        vec![vec![Datum::Null], vec![int(1)], vec![Datum::Null]]
    );
    assert_eq!(
        query(
            &world,
            "MATCH (e {kala_handle: 'edwin'}) AT TICK -210 UNWIND e.alias AS x RETURN x"
        ),
        vec![
            vec![string("Ed")],
            vec![string("Eddie")],
            vec![string("the Lost Knight")]
        ]
    );
}
