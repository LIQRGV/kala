//! Bounded trails: each hop seeks an endpoint, and relationships are unique.
use kala_core::{Value, World};
use kala_fixture::load_fixture;
use kala_query::{Datum, Limits, Params, QueryError, RunOptions, run, run_with};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

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
fn derive_fixture_reachability_before_pinning_trails() {
    let world = load_fixture(VARROW).unwrap();
    let p = kala_datalog::prepare(&world, "[:find ?h :at -208 :rules [[(reach ?a ?b) [?a :ally_of ?b]] [(reach ?a ?b) [?a :ally_of ?c] [?c :ally_of ?b]]] :where (reach :edwin ?b) [?b :kala_handle ?h]]").unwrap();
    let rows = run(&world, &p.plan, &Params::new(), &Limits::default())
        .unwrap()
        .rows;
    assert_eq!(
        rows,
        vec![
            vec![string("edwin")],
            vec![string("greg")],
            vec![string("mara")],
            vec![string("tomas")]
        ]
    );
    // The two-hop Datalog walks all return to Edwin using the same stored edge.
    // A Cypher trail forbids those three backtracks, leaving only the one-hop
    // Greg, Mara and Tomas paths, one each. Pin both the relation and derivation.
    let p = kala_datalog::prepare(&world, "[:find ?h ?k :at -208 :where [:edwin :ally_of ?a] [?a :ally_of ?b] [?a :kala_handle ?h] [?b :kala_handle ?k]]").unwrap();
    assert_eq!(
        run(&world, &p.plan, &Params::new(), &Limits::default())
            .unwrap()
            .rows,
        vec![
            vec![string("greg"), string("edwin")],
            vec![string("mara"), string("edwin")],
            vec![string("tomas"), string("edwin")]
        ]
    );
}

#[test]
fn bounded_fixture_paths_exclude_the_three_same_edge_backtracks() {
    let world = load_fixture(VARROW).unwrap();
    assert_eq!(
        query(
            &world,
            "MATCH (e {kala_handle: 'edwin'})-[:ally_of*1..2]-(x) AT TICK -208 RETURN x.kala_handle, count(*) ORDER BY x.kala_handle"
        ),
        vec![
            vec![string("greg"), int(1)],
            vec![string("mara"), int(1)],
            vec![string("tomas"), int(1)]
        ]
    );
    assert_eq!(
        query(
            &world,
            "MATCH (e {kala_handle: 'edwin'})-[:ally_of*0..1]-(x) AT TICK -208 RETURN x.kala_handle ORDER BY x.kala_handle"
        ),
        vec![
            vec![string("edwin")],
            vec![string("greg")],
            vec![string("mara")],
            vec![string("tomas")]
        ]
    );
    assert_eq!(
        query(
            &world,
            "MATCH (e {kala_handle: 'edwin'})-[:ally_of*1..1]-(x) AT TICK -208 RETURN x ORDER BY x"
        ),
        query(
            &world,
            "MATCH (e {kala_handle: 'edwin'})-[:ally_of]-(x) AT TICK -208 RETURN x ORDER BY x"
        )
    );
}

#[test]
fn paths_functions_fixed_zero_hop_and_relationship_lists() {
    let world = load_fixture(VARROW).unwrap();
    let e: Datum = Value::Ref(world.entity_by_handle("edwin").unwrap()).into();
    assert_eq!(
        query(
            &world,
            "MATCH p = (e {kala_handle: 'edwin'}) RETURN length(p), nodes(p), relationships(p)"
        ),
        vec![vec![
            int(0),
            Datum::List(vec![e.clone()]),
            Datum::List(vec![])
        ]]
    );
    assert_eq!(
        query(
            &world,
            "MATCH p = (e {kala_handle: 'edwin'})-[r:ally_of*0..0]-(x) RETURN e, x, length(p), nodes(p), r, size(r)"
        ),
        vec![vec![
            e.clone(),
            e.clone(),
            int(0),
            Datum::List(vec![e]),
            Datum::List(vec![]),
            int(0)
        ]]
    );
    let rows = query(
        &world,
        "MATCH p = (e {kala_handle: 'edwin'})-[r:ally_of*1..2]->(x) AT TICK -208 RETURN p, length(p), nodes(p), relationships(p), r, size(r)",
    );
    assert_eq!(rows.len(), 3);
    for row in rows {
        assert_eq!(row[1], int(1));
        assert!(matches!(&row[2], Datum::List(nodes) if nodes.len() == 2));
        assert_eq!(row[3], row[4]);
        assert_eq!(row[5], int(1));
    }
    assert_eq!(
        query(
            &world,
            "MATCH p = (e {kala_handle: 'edwin'})-[r:present]->(c) AT TICK -240 RETURN length(p), r.disguised, relationships(p)"
        ),
        {
            let rows = query(
                &world,
                "MATCH (e {kala_handle: 'edwin'})-[r:present]->(c) AT TICK -240 RETURN r",
            );
            vec![vec![
                int(1),
                Value::Bool(true).into(),
                Datum::List(vec![rows[0][0].clone()]),
            ]]
        }
    );
}

#[test]
fn unmatched_optional_path_and_its_functions_are_null() {
    let world = load_fixture(VARROW).unwrap();
    assert_eq!(
        query(
            &world,
            "OPTIONAL MATCH p = (a {kala_handle: 'nobody'})-[r:ally_of*0..1]-(b) RETURN p, r, length(p), nodes(p), relationships(p)"
        ),
        vec![vec![Datum::Null; 5]]
    );
}

#[test]
fn relationship_uniqueness_crosses_fixed_and_variable_patterns_in_one_match() {
    let world = load_fixture(VARROW).unwrap();
    for text in [
        "MATCH (a {kala_handle: 'edwin'})-[r1:ally_of]-(b), (b)-[r2:ally_of*1..1]-(c) AT TICK -208 RETURN c",
        "MATCH (a {kala_handle: 'edwin'})-[r1:ally_of*1..1]-(b), (b)-[r2:ally_of]-(c) AT TICK -208 RETURN c",
        "MATCH (a {kala_handle: 'edwin'})-[r1:ally_of*1..1]-(b), (b)-[r2:ally_of*1..1]-(c) AT TICK -208 RETURN c",
    ] {
        assert!(query(&world, text).is_empty(), "{text}");
    }
    assert_eq!(
        query(
            &world,
            "MATCH (a {kala_handle: 'edwin'})-[r1:ally_of*1..1]-(b) AT TICK -208 MATCH (b)-[r2:ally_of*1..1]-(c) RETURN c.kala_handle"
        ),
        vec![vec![string("edwin")]; 3]
    );
}

fn chain_world(extra_pairs: usize, cycle: bool) -> World {
    let mut yaml = String::from(
        "schema:\n  node_types: [NODE]\n  attributes:\n    - { name: link, type: ref, cardinality: many, direction: directed, inverse: back }\nentities:\n  NODE: [a, b, c, d",
    );
    for i in 0..extra_pairs {
        yaml.push_str(&format!(", p{i}, q{i}"));
    }
    yaml.push_str("]\ntransactions:\n  - tick: 0\n    ops:\n      - assert: [a, link, b]\n      - assert: [b, link, c]\n      - assert: [c, link, d]\n");
    if cycle {
        yaml.push_str("      - assert: [d, link, a]\n");
    }
    for i in 0..extra_pairs {
        yaml.push_str(&format!("      - assert: [p{i}, link, q{i}]\n"));
    }
    load_fixture(&yaml).unwrap()
}

#[test]
fn variable_hops_seek_without_scanning_disconnected_pairs() {
    let world = chain_world(2000, false);
    let p = kala_cypher::prepare(
        &world,
        "MATCH (a {kala_handle: 'a'})-[:link*3..3]->(b) RETURN b.kala_handle",
    )
    .unwrap();
    let result = run_with(
        &world,
        &p.plan,
        &Params::new(),
        RunOptions::new(Limits {
            max_steps: 500,
            ..Limits::default()
        }),
    );
    assert!(
        result.is_ok(),
        "three adjacent seeks must fit 500 steps independently of 2000 other pairs: {result:?}"
    );
    let (rows, usage) = result.unwrap();
    assert_eq!(rows.rows, vec![vec![string("d")]]);
    assert!(usage.steps < 500);
}

#[test]
fn inverse_direction_and_repeated_nodes_follow_trail_semantics() {
    let world = chain_world(0, true);
    for text in [
        "MATCH (a {kala_handle: 'd'})-[:back*3..3]->(b) RETURN b.kala_handle",
        "MATCH (a {kala_handle: 'd'})<-[:link*3..3]-(b) RETURN b.kala_handle",
    ] {
        assert_eq!(query(&world, text), vec![vec![string("a")]]);
    }
    assert_eq!(
        query(
            &world,
            "MATCH p = (a {kala_handle: 'a'})-[:link*4..5]->(b) RETURN b.kala_handle, length(p), size(nodes(p)), size(relationships(p))"
        ),
        vec![vec![string("a"), int(4), int(5), int(4)]]
    );
    let expected_nodes = ["a", "b", "c"]
        .into_iter()
        .map(|h| Datum::from(Value::Ref(world.entity_by_handle(h).unwrap())))
        .collect();
    assert_eq!(
        query(
            &world,
            "MATCH p = (a {kala_handle: 'a'})-[:link]->(b)<-[:back]-(c) RETURN length(p), nodes(p)"
        ),
        vec![vec![int(2), Datum::List(expected_nodes)]]
    );
    let rows = query(
        &world,
        "MATCH p = (a {kala_handle: 'a'})-[:link*1..2]->(b) UNWIND [1, 1] AS x RETURN DISTINCT p ORDER BY p",
    );
    assert_eq!(rows.len(), 2);
    assert_ne!(rows[0], rows[1]);
}

#[test]
fn each_variable_hop_uses_the_clause_tick_and_matches_fixed_retractions() {
    let world = load_fixture(VARROW).unwrap();
    for tick in [i64::MIN, -301, -300, -210, -208, -200, 0, 700, i64::MAX] {
        let p = kala_datalog::prepare(
            &world,
            &format!("[:find ?a :at {tick} :where [:edwin :ally_of ?a]]"),
        )
        .unwrap();
        let expected = run(&world, &p.plan, &Params::new(), &Limits::default())
            .unwrap()
            .rows;
        let p = kala_cypher::prepare(
            &world,
            "MATCH (e {kala_handle: 'edwin'})-[:ally_of*1..1]->(a) AT TICK $t RETURN DISTINCT a",
        )
        .unwrap();
        let params: Params = [("t".into(), Value::I64(tick))].into();
        assert_eq!(
            run(&world, &p.plan, &params, &Limits::default())
                .unwrap()
                .rows,
            expected,
            "tick {tick}"
        );
    }
}

#[test]
fn variable_relationship_property_map_filters_every_hop() {
    let world = load_fixture("schema:\n  node_types: [NODE]\n  attributes:\n    - { name: link, type: ref, cardinality: many }\n    - { name: open, type: bool }\nentities:\n  NODE: [a, b, c, d]\ntransactions:\n  - tick: -5\n    ops:\n      - assert: [a, link, b]\n      - assert: [b, link, c]\n      - assert: [c, link, d]\n      - assert: [[a, link, b], open, true]\n      - assert: [[b, link, c], open, false]\n      - assert: [[c, link, d], open, true]\n").unwrap();
    assert_eq!(
        query(
            &world,
            "MATCH (a {kala_handle: 'a'})-[:link*1..3 {open: true}]->(b) RETURN b.kala_handle"
        ),
        vec![vec![string("b")]]
    );
    assert_eq!(
        query(
            &world,
            "MATCH p = (a {kala_handle: 'a'})-[:link*0..3 {open: false}]->(b) RETURN b.kala_handle, length(p)"
        ),
        vec![vec![string("a"), int(0)]]
    );
}

fn dense_world() -> World {
    let nodes = (0..30)
        .map(|i| format!("n{i}"))
        .collect::<Vec<_>>()
        .join(",");
    let mut yaml = format!(
        "schema:\n  node_types: [NODE]\n  attributes:\n    - {{ name: link, type: ref, cardinality: many, direction: bidirectional }}\nentities:\n  NODE: [{nodes}]\ntransactions:\n  - tick: 0\n    ops:\n"
    );
    for a in 0..30 {
        for b in a + 1..30 {
            yaml.push_str(&format!("      - assert: [n{a}, link, n{b}]\n"));
        }
    }
    load_fixture(&yaml).unwrap()
}

#[test]
fn dense_expansion_fails_with_a_budget_error_without_truncation() {
    let world = dense_world();
    let p = kala_cypher::prepare(
        &world,
        "MATCH p = (a {kala_handle: 'n0'})-[:link*1..10]-(b) RETURN p",
    )
    .unwrap();
    let start = Instant::now();
    let result = run(&world, &p.plan, &Params::new(), &Limits::default());
    assert!(
        matches!(
            result,
            Err(QueryError::StepBudget { .. } | QueryError::RowLimit { .. })
        ),
        "{result:?}"
    );
    if !cfg!(debug_assertions) {
        assert!(start.elapsed() < Duration::from_secs(5));
    }
    for limits in [
        Limits {
            max_steps: 2000,
            max_rows: usize::MAX,
            ..Limits::default()
        },
        Limits {
            max_bytes: 4096,
            ..Limits::default()
        },
    ] {
        let result = run(&world, &p.plan, &Params::new(), &limits);
        assert!(
            matches!(
                result,
                Err(QueryError::StepBudget { .. } | QueryError::MemoryBudget { .. })
            ),
            "{result:?}"
        );
    }
}

#[test]
fn dense_expansion_observes_cancellation_from_another_thread() {
    let world = dense_world();
    let p = kala_cypher::prepare(
        &world,
        "MATCH p = (a {kala_handle: 'n0'})-[:link*1..10]-(b) RETURN p",
    )
    .unwrap();
    let cancel = AtomicBool::new(false);
    std::thread::scope(|scope| {
        scope.spawn(|| {
            std::thread::sleep(Duration::from_millis(10));
            cancel.store(true, Ordering::Relaxed);
        });
        let start = Instant::now();
        let result = run_with(
            &world,
            &p.plan,
            &Params::new(),
            RunOptions::new(Limits {
                max_rows: usize::MAX,
                max_steps: u64::MAX,
                ..Limits::default()
            })
            .with_cancel(&cancel),
        );
        assert!(matches!(result, Err(QueryError::Cancelled)), "{result:?}");
        assert!(start.elapsed() < Duration::from_secs(3));
    });
}
