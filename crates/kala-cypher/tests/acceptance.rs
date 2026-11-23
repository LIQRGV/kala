//! Read-subset behavior, including semantic differences from Datalog.
use kala_core::{Value, World, load_fixture};
use kala_query::{Datum, Limits, Params, QueryError, run, run_with_cancel};
use std::sync::atomic::AtomicBool;

const VARROW: &str = include_str!("../../kala-core/tests/fixtures/varrow.yaml");
fn world() -> World {
    load_fixture(VARROW).unwrap()
}
fn query(w: &World, text: &str) -> Vec<Vec<Datum>> {
    query_params(w, text, Params::new())
}
fn query_params(w: &World, text: &str, params: Params) -> Vec<Vec<Datum>> {
    let prepared = kala_cypher::prepare(w, text).unwrap_or_else(|e| panic!("{text}: {e}"));
    run(w, &prepared.plan, &params, &Limits::default())
        .unwrap_or_else(|e| panic!("{text}: {e}"))
        .rows
}
fn strings(names: &[&str]) -> Vec<Vec<Datum>> {
    names
        .iter()
        .map(|name| vec![Value::Str((*name).into()).into()])
        .collect()
}
fn id(w: &World, handle: &str) -> Datum {
    Value::Ref(w.entity_by_handle(handle).unwrap()).into()
}

#[test]
fn ordering_skip_limit_and_aliases() {
    let w = world();
    assert_eq!(
        query(
            &w,
            "MATCH (c:CHARACTER) RETURN c.name ORDER BY c.name LIMIT 3"
        ),
        strings(&["Edwin", "Greg", "Hild"])
    );
    assert_eq!(
        query(
            &w,
            "MATCH (c:CHARACTER) RETURN c.name ORDER BY c.name SKIP 2 LIMIT 2"
        ),
        strings(&["Hild", "Lisa"])
    );
    assert_eq!(
        query(
            &w,
            "MATCH (c:CHARACTER) RETURN c.name ORDER BY c.name DESC LIMIT 3"
        ),
        strings(&["the Grey King", "Wren Åberg", "Villager 24"])
    );
    assert_eq!(
        query(
            &w,
            "MATCH (c:CHARACTER) RETURN c.name AS title ORDER BY title LIMIT 3"
        ),
        strings(&["Edwin", "Greg", "Hild"])
    );
    assert_eq!(
        query_params(
            &w,
            "MATCH (c:CHARACTER) RETURN c.name ORDER BY c.name SKIP $s LIMIT $n",
            [("s".into(), Value::I64(2)), ("n".into(), Value::I64(2))].into()
        ),
        strings(&["Hild", "Lisa"])
    );
    assert!(query(&w, "MATCH (c) RETURN c LIMIT 0").is_empty());
}

#[test]
fn node_type_extra_labels_unknown_names_and_all_node_scan() {
    let w = world();
    assert_eq!(
        query(&w, "MATCH (p:City) RETURN p.name ORDER BY p.name"),
        strings(&["Ashford", "Varrow"])
    );
    assert_eq!(
        query(&w, "MATCH (p:PLACE:City) RETURN p.name ORDER BY p.name"),
        strings(&["Ashford", "Varrow"])
    );
    assert!(query(&w, "MATCH (p:CHARACTER:City) RETURN p").is_empty());
    assert!(query(&w, "MATCH (p:NoSuchLabel) RETURN p").is_empty());
    assert!(query(&w, "MATCH (p)-[:no_such_relationship]->(q) RETURN q").is_empty());
    assert_eq!(query(&w, "MATCH (n) RETURN n").len(), 50);
    assert_eq!(query(&w, "MATCH () RETURN 1").len(), 50);
    assert_eq!(query(&w, "MATCH () RETURN DISTINCT 1").len(), 1);
}

#[test]
fn historical_canon_and_two_moments() {
    let w = world();
    assert_eq!(
        query(
            &w,
            "MATCH (g {kala_handle: 'greg'})-[:located_in]->(p) AT TICK 601 CANON 8 RETURN p.name ORDER BY p.name"
        ),
        strings(&["Ashford", "Varrow"])
    );
    assert_eq!(
        query(
            &w,
            "MATCH (g {kala_handle: 'greg'})-[:located_in]->(p) AT TICK 601 RETURN p.name ORDER BY p.name"
        ),
        strings(&["Ashford"])
    );
    assert_eq!(
        query(
            &w,
            "MATCH (e {kala_handle: 'edwin'})-[:ally_of]->(a) AT TICK -280 MATCH (e)-[:ally_of]->(a) AT TICK -200 RETURN a.name"
        ),
        strings(&["Tomas"])
    );
    assert_eq!(
        query(
            &w,
            "MATCH (e {kala_handle: 'edwin'}) AT TICK -210 MATCH (g {kala_handle: 'greg'}) AT TICK 601 RETURN e.name, g.located_in"
        ),
        vec![vec![Value::Str("Edwin".into()).into(), id(&w, "ashford")]]
    );
    assert_eq!(
        query_params(
            &w,
            "MATCH (g {kala_handle: 'greg'})-[:located_in]->(p) AT TICK $t CANON $c RETURN p.name ORDER BY p.name",
            [("t".into(), Value::I64(601)), ("c".into(), Value::I64(8))].into()
        ),
        strings(&["Ashford", "Varrow"])
    );
}

#[test]
fn bags_preserve_projected_multiplicity_and_undirected_edges_once() {
    let w = world();
    let undirected = query(
        &w,
        "MATCH (a)-[:ally_of]-(b) AT TICK -208 RETURN a.name ORDER BY a.name",
    );
    let directed = query(
        &w,
        "MATCH (a)-[:ally_of]->(b) AT TICK -208 RETURN a.name ORDER BY a.name",
    );
    assert_eq!(undirected, directed);
    assert_eq!(directed.len(), 30);
    assert_eq!(
        directed
            .iter()
            .filter(|r| **r == strings(&["Edwin"])[0])
            .count(),
        3
    );
    assert_eq!(
        query(
            &w,
            "MATCH (a)-[:ally_of]-(b) AT TICK -208 RETURN DISTINCT a.name"
        )
        .len(),
        28
    );
}

#[test]
fn missing_property_null_and_three_valued_logic() {
    let w = world();
    assert_eq!(
        format!(
            "{:?}",
            query(&w, "MATCH (c {kala_handle: 'kael'}) RETURN c.biography")
        ),
        "[[Null]]"
    );
    for pred in [
        "c.nosuchkey = 1",
        "NOT (c.nosuchkey = 1)",
        "null = null",
        "NOT null",
        "true AND null",
        "false OR null",
        "true XOR null",
        "null IS NOT NULL",
    ] {
        assert!(
            query(
                &w,
                &format!("MATCH (c {{kala_handle: 'kael'}}) WHERE {pred} RETURN c")
            )
            .is_empty(),
            "{pred}"
        );
    }
    for pred in [
        "c.nosuchkey IS NULL",
        "NOT (false AND null)",
        "true OR null",
        "true XOR false",
        "NOT false",
        "NOT NOT true",
        "null IS NULL",
    ] {
        assert_eq!(
            query(
                &w,
                &format!("MATCH (c {{kala_handle: 'kael'}}) WHERE {pred} RETURN c")
            ),
            vec![vec![id(&w, "kael")]],
            "{pred}"
        );
    }
}

#[test]
fn relationship_variable_properties_map_and_reverse() {
    let w = world();
    let rows = query(
        &w,
        "MATCH (e {kala_handle: 'edwin'})-[r:present]->(p) AT TICK -240 RETURN r, r.disguised, p.name",
    );
    assert_eq!(rows.len(), 1);
    let Value::Ref(edge) = rows[0][0].as_value().unwrap() else {
        panic!("edge ref")
    };
    assert_eq!(w.format_id(*edge), "edge#98");
    assert_eq!(rows[0][1], Value::Bool(true));
    assert_eq!(rows[0][2], Value::Str("Carnath".into()));
    assert_eq!(
        query(
            &w,
            "MATCH (p)<-[r:present {disguised: true}]-(e {kala_handle: 'edwin'}) AT TICK -240 RETURN p.name"
        ),
        strings(&["Carnath"])
    );
}

#[test]
fn properties_are_lists_for_many_and_overlapping_one() {
    let w = world();
    let aliases = query(
        &w,
        "MATCH (e {kala_handle: 'edwin'}) AT TICK -210 RETURN e.alias",
    );
    assert_eq!(
        format!("{:?}", aliases),
        "[[List([Value(Str(\"Ed\")), Value(Str(\"Eddie\")), Value(Str(\"the Lost Knight\"))])]]"
    );
    let places = query(
        &w,
        "MATCH (g {kala_handle: 'greg'}) AT TICK 601 CANON 8 RETURN g.located_in",
    );
    assert_eq!(
        format!("{:?}", places[0][0]),
        format!("List({:?})", vec![id(&w, "varrow"), id(&w, "ashford")])
    );
    assert_eq!(
        query(
            &w,
            "MATCH (e {kala_handle: 'edwin'}) AT TICK -210 WHERE e.alias = e.alias RETURN e.name"
        ),
        strings(&["Edwin"])
    );
}

#[test]
fn relationship_reuse_is_forbidden_within_match_allowed_across_matches() {
    let w = world();
    assert!(query(&w, "MATCH (e {kala_handle: 'edwin'})-[r:present]->(p)<-[s:present]-(e) AT TICK -240 RETURN e").is_empty());
    assert!(query(&w, "MATCH (e {kala_handle: 'edwin'})-[r:present]->(p), (e)-[s:present]->(p) AT TICK -240 RETURN e").is_empty());
    assert_eq!(
        query(
            &w,
            "MATCH (e {kala_handle: 'edwin'})-[r:present]->(p) AT TICK -240 MATCH (e)-[s:present]->(p) RETURN e"
        ),
        vec![vec![id(&w, "edwin")]]
    );
}

#[test]
fn edge_ticks_retractions_and_bound_parameters() {
    let w = world();
    for t in [
        i64::MIN,
        -301,
        -300,
        -281,
        -280,
        -211,
        -210,
        -209,
        -208,
        -201,
        -200,
        0,
        700,
        i64::MAX,
    ] {
        let dl = kala_datalog::prepare(
            &w,
            &format!("[:find ?a :at {t} :where [:edwin :ally_of ?a]]"),
        )
        .unwrap();
        let expected = run(&w, &dl.plan, &Params::new(), &Limits::default())
            .unwrap()
            .rows;
        let text = "MATCH (e {kala_handle: 'edwin'})-[:ally_of]->(a) AT TICK $t RETURN DISTINCT a";
        assert_eq!(
            query_params(&w, text, [("t".into(), Value::I64(t))].into()),
            expected,
            "tick {t}"
        );
    }
}

#[test]
fn new_operators_respect_row_step_memory_and_cancellation_limits() {
    let w = world();
    for text in [
        "MATCH (n) RETURN n",
        "MATCH (c:CHARACTER) RETURN c.name ORDER BY c.name LIMIT 1",
        "MATCH (a)-[r:ally_of]-(b) AT TICK -208 RETURN a.name",
    ] {
        let plan = kala_cypher::prepare(&w, text).unwrap().plan;
        assert!(
            matches!(
                run(
                    &w,
                    &plan,
                    &Params::new(),
                    &Limits {
                        max_rows: 1,
                        ..Limits::default()
                    }
                ),
                Err(QueryError::RowLimit { .. })
            ),
            "{text}"
        );
        assert!(
            matches!(
                run(
                    &w,
                    &plan,
                    &Params::new(),
                    &Limits {
                        max_steps: 1,
                        ..Limits::default()
                    }
                ),
                Err(QueryError::StepBudget { .. })
            ),
            "{text}"
        );
        assert!(
            matches!(
                run(
                    &w,
                    &plan,
                    &Params::new(),
                    &Limits {
                        max_bytes: 1,
                        ..Limits::default()
                    }
                ),
                Err(QueryError::MemoryBudget { .. })
            ),
            "{text}"
        );
        assert!(
            run_with_cancel(
                &w,
                &plan,
                &Params::new(),
                &Limits::default(),
                &AtomicBool::new(true)
            )
            .is_err()
        );
    }
}

#[test]
fn embedded_nullable_and_list_parameters_keep_cypher_semantics() {
    let w = world();
    let p = kala_cypher::prepare(
        &w,
        "MATCH (e {kala_handle: 'edwin'}) AT TICK -210 WHERE e.alias = $aliases RETURN $value",
    )
    .unwrap();
    let mut params = kala_query::QueryParams::new();
    let aliases = Datum::List(
        ["Ed", "Eddie", "the Lost Knight"]
            .iter()
            .map(|v| Datum::from(Value::Str((*v).into())))
            .collect(),
    );
    params.insert("aliases".into(), aliases.clone());
    params.insert("value".into(), Datum::Null);
    assert_eq!(
        run(&w, &p.plan, &params, &Limits::default()).unwrap().rows,
        vec![vec![Datum::Null]]
    );
    params.insert("value".into(), aliases.clone());
    assert_eq!(
        run(&w, &p.plan, &params, &Limits::default()).unwrap().rows,
        vec![vec![aliases]]
    );
    params.insert(
        "aliases".into(),
        Datum::List(
            ["the Lost Knight", "Eddie", "Ed"]
                .iter()
                .map(|v| Datum::from(Value::Str((*v).into())))
                .collect(),
        ),
    );
    assert!(
        run(&w, &p.plan, &params, &Limits::default())
            .unwrap()
            .rows
            .is_empty()
    );
}

#[test]
fn return_aliases_do_not_overwrite_match_bindings_in_other_return_items() {
    let w = world();
    assert_eq!(
        query(
            &w,
            "MATCH (e {kala_handle: 'edwin'}) AT TICK -210 RETURN e.name AS e, e.kala_handle AS handle ORDER BY e"
        ),
        vec![vec![
            Datum::from(Value::Str("Edwin".into())),
            Datum::from(Value::Str("edwin".into()))
        ]]
    );
}

#[test]
fn parameter_list_equality_propagates_null_until_inequality_is_known() {
    // Lists compare their corresponding elements; unknown equality stays NULL.
    // https://neo4j.com/docs/cypher-manual/current/values-and-types/ordering-equality-comparison/
    let w = world();
    let n = Datum::Null;
    let one = Datum::from(Value::I64(1));
    let two = Datum::from(Value::I64(2));
    for (a, b, expected) in [
        (vec![n.clone()], vec![n.clone()], None),
        (
            vec![n.clone(), one.clone()],
            vec![one.clone(), one.clone()],
            None,
        ),
        (
            vec![n.clone(), one.clone()],
            vec![n.clone(), two],
            Some(false),
        ),
        (vec![n.clone()], vec![n.clone(), one], Some(false)),
        (
            vec![Datum::List(vec![n.clone()])],
            vec![Datum::List(vec![n])],
            None,
        ),
    ] {
        let mut params = kala_query::QueryParams::new();
        params.insert("a".into(), Datum::List(a.clone()));
        params.insert("b".into(), Datum::List(b.clone()));
        for (expression, want) in [
            ("$a = $b", expected == Some(true)),
            ("NOT ($a = $b)", expected == Some(false)),
        ] {
            let p = kala_cypher::prepare(
                &w,
                &format!("MATCH (e {{kala_handle: 'edwin'}}) WHERE {expression} RETURN e"),
            )
            .unwrap();
            let result = run(&w, &p.plan, &params, &Limits::default()).unwrap();
            assert_eq!(
                result.rows.len(),
                usize::from(want),
                "{a:?} vs {b:?}, {expression}"
            );
        }
    }
}
