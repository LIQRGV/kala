//! Bounded grammar additions and unsupported forms remain explicit errors.
use proptest::prelude::*;

#[test]
fn new_clause_function_and_procedure_forms_parse() {
    for text in [
        "OPTIONAL MATCH (a) WHERE false RETURN a",
        "MATCH (a) WITH a AS x ORDER BY x SKIP 1 LIMIT 2 WHERE true RETURN x",
        "MATCH (a) WITH * RETURN a",
        "UNWIND [1, null, [2, 3], $p] AS x RETURN x",
        "MATCH (a) RETURN count(*), count(DISTINCT a), collect(a), collect(DISTINCT a), min(a), max(a), sum(a), avg(a)",
        "MATCH p = (a)-[r:ally_of*0..10 {key: $value}]->(b) RETURN p, nodes(p), relationships(p), length(p), size(r)",
        "MATCH p = (a)-[:ally_of]->(b)<-[:present]-(c) RETURN p",
        "MATCH p = (a) RETURN p",
        "CALL db.labels()",
        "CALL db.labels() YIELD label AS l WHERE l = 'City' WITH l RETURN l",
        "CALL dbms.components() YIELD name, versions, edition RETURN name, versions, edition",
        "CALL db.relationshipTypes()",
        "CALL db.propertyKeys()",
        "SHOW DATABASES",
        "SHOW DATABASE varrow",
        "SHOW DEFAULT DATABASE",
        "SHOW HOME DATABASE YIELD name WHERE name = 'varrow'",
    ] {
        let result = kala_cypher::parse(text);
        assert!(result.is_ok(), "{text}: {result:?}");
    }
}

#[test]
fn every_unbounded_or_excessive_path_names_the_ten_hop_maximum() {
    for relationship in [
        "*",
        "*2..",
        "*..",
        "*1..11",
        "*..11",
        "*11",
        "*1..18446744073709551615",
    ] {
        let query = format!("MATCH (a)-[:ally_of{relationship}]-(b) RETURN b");
        let err = kala_cypher::parse(&query).unwrap_err().to_string();
        assert!(
            err.contains("upper bound") && err.contains("10"),
            "{relationship}: {err}"
        );
    }
    for relationship in ["*3..2", "*-1..2", "*1.5..2", "*1..-1"] {
        assert!(
            kala_cypher::parse(&format!("MATCH (a)-[:ally_of{relationship}]-(b) RETURN b"))
                .is_err()
        );
    }
}

#[test]
fn call_show_yield_and_unwind_malformed_forms_are_errors() {
    for query in [
        "CALL db.labels(1)",
        "CALL db.labels($x)",
        "CALL db.labels() YIELD *",
        "CALL db.labels() YIELD",
        "SHOW PROCEDURES",
        "SHOW FUNCTIONS",
        "SHOW USERS",
        "SHOW DATABASES YIELD *",
        "UNWIND [1,2] RETURN x",
        "UNWIND [1,,2] AS x RETURN x",
        "UNWIND [1,2 AS x RETURN x",
        "MATCH (a) WITH RETURN a",
        "MATCH (a) RETURN shortestPath(a)",
        "MATCH (a) RETURN a UNION ALL MATCH (b) RETURN b",
    ] {
        assert!(kala_cypher::parse(query).is_err(), "{query}");
    }
}

#[test]
fn lists_clause_chains_yield_items_and_path_patterns_are_bounded() {
    let deep = format!(
        "UNWIND {}1{} AS x RETURN x",
        "[".repeat(1000),
        "]".repeat(1000)
    );
    assert!(kala_cypher::parse(&deep).is_err());
    let list = format!("UNWIND [{}] AS x RETURN x", vec!["1"; 1024].join(","));
    assert!(kala_cypher::parse(&list).is_err());
    let withs = format!("MATCH (n){} RETURN n", " WITH n".repeat(1024));
    assert!(kala_cypher::parse(&withs).is_err());
    let yields = (0..1024)
        .map(|i| format!("c{i}"))
        .collect::<Vec<_>>()
        .join(",");
    assert!(kala_cypher::parse(&format!("CALL db.labels() YIELD {yields} RETURN c0")).is_err());
    let path = format!(
        "MATCH p = (a){} RETURN p",
        "-[:ally_of*1..10]-()".repeat(1024)
    );
    assert!(kala_cypher::parse(&path).is_err());
    let params = (0..1024)
        .map(|i| format!("$p{i}"))
        .collect::<Vec<_>>()
        .join(",");
    assert!(kala_cypher::parse(&format!("UNWIND [{params}] AS x RETURN x")).is_err());
}

#[test]
fn errors_do_not_echo_unbounded_identifiers() {
    let world = kala_fixture::load_fixture(
        "schema:\n  node_types: [NODE]\n  attributes: []\nentities: {}\ntransactions: []\n",
    )
    .unwrap();
    let name = "long_name".repeat(2000);
    for query in [
        format!("MATCH (n) RETURN {name}"),
        format!("CALL db.labels() YIELD {name} RETURN {name}"),
    ] {
        let err = kala_cypher::prepare(&world, &query)
            .unwrap_err()
            .to_string();
        assert!(
            err.chars().count() <= 1024,
            "error grew to {} characters",
            err.chars().count()
        );
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]
    #[test]
    fn arbitrary_slice14_fragments_never_panic(text in ".{0,2048}") {
        for query in [format!("UNWIND [{text}] AS x RETURN x"), format!("MATCH p = (a)-[:link*{text}]-(b) RETURN p"), format!("CALL {text}()"), format!("MATCH (a) WITH {text} RETURN a")] {
            let _ = kala_cypher::parse(&query);
        }
    }
}
