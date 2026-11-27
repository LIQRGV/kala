//! Write syntax is bounded and keeps diagnostics actionable.
use proptest::prelude::*;

fn parse(text: &str) -> Result<(), String> {
    kala_cypher::parse_statement(text)
        .map(|_| ())
        .map_err(|e| e.to_string())
}

#[test]
fn supported_write_clauses_parse() {
    for text in [
        "CREATE (n:CHARACTER:Scout {kala_handle: $h, alias:['one', 'two']})",
        "CREATE (a:T)-[r:link {rank:$rank}]->(b:T) SET r.rank = 3",
        "MATCH (n) AT TICK -200 SET n.x = null, n.y = $y AT TICK 5",
        "MATCH (n) SET n += {x: null, y: 3}, n:Leader REMOVE n:Scout, n.z RETURN n",
        "MATCH (a)-[r:link]->(b) DELETE r",
        "OPTIONAL MATCH (n:Absent) DETACH DELETE n",
        "MERGE (n:TYPE {kala_handle: $h}) ON CREATE SET n.x = 1 ON MATCH SET n.x = 2",
        "MATCH (a), (b) MERGE (a)-[r:link]->(b) ON CREATE SET r.x = 3",
        "UNWIND $handles AS h CREATE (n:T {kala_handle:h}) RETURN n, h",
        "SET CONCURRENCY_MODE = 'weak'",
    ] {
        assert_eq!(parse(text), Ok(()), "{text}");
    }
}

#[test]
fn unsupported_write_shapes_name_the_reason() {
    for (text, hint) in [
        (
            "CREATE CONSTRAINT c FOR (n:T) REQUIRE n.id IS :: INTEGER",
            "datalog",
        ),
        ("CREATE (a:T)-[:link]-(b:T)", "direction"),
        ("MATCH (n) SET n = {x:1}", "set the properties"),
        ("CREATE (n:T) WITH n MATCH (x) RETURN x", "not supported"),
        ("MERGE (n:T {x:1})", "kala_handle"),
        ("MERGE (n:T:Other {kala_handle:'n'})", "kala_handle"),
        ("MERGE (n:T {kala_handle:'n', x:1})", "kala_handle"),
        ("MATCH (a), (b) MERGE (a)-[r:link {x:1}]->(b)", "SET"),
        ("CREATE (n:T) RETURN count(*)", "not supported"),
        ("CREATE (n:T) RETURN n ORDER BY n", "not supported"),
        ("CREATE (n:T) RETURN n LIMIT 2", "not supported"),
        ("CREATE (n:T) RETURN n SKIP 1", "not supported"),
        ("MATCH (n) CANON 1 SET n.x = 1", "CANON"),
        (
            "MERGE (n:TYPE {kala_handle:'n'}) ON CREATE SET n.x=1 ON CREATE SET n.x=2",
            "ON",
        ),
        ("MERGE (a)-[:link]->(b)", "not supported"),
        (
            "MATCH (a), (b), (c) MERGE (a)-[:link]->(b)-[:link]->(c)",
            "not supported",
        ),
        ("MATCH p=(a)-[:link]->(b) DELETE p", "path"),
        ("SHOW DATABASES CREATE (:T)", "statement"),
        ("FOREACH (i IN [1] | CREATE (:T))", "FOREACH"),
        ("LOAD CSV FROM 'file' AS row CREATE (:T)", "LOAD"),
        ("CALL { CREATE (:T) } RETURN 1", "subquer"),
    ] {
        let error = parse(text).expect_err(text);
        assert!(
            error.to_lowercase().contains(&hint.to_lowercase()),
            "{text}: {error}"
        );
    }
}

#[test]
fn write_maps_items_and_clause_chains_are_bounded() {
    let items = (0..2000)
        .map(|i| format!("x{i}:0"))
        .collect::<Vec<_>>()
        .join(",");
    let set = (0..2000)
        .map(|i| format!("n.x{i}=0"))
        .collect::<Vec<_>>()
        .join(",");
    for query in [
        format!("CREATE (:T {{{items}}})"),
        format!("MATCH (n) SET {set}"),
        format!("MATCH (n) {}", "SET n.x=1 ".repeat(2000)),
        format!("CREATE (:T {{x:{}0{}}})", "[".repeat(300), "]".repeat(300)),
    ] {
        assert!(parse(&query).is_err(), "accepted an excessive write");
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]
    #[test]
    fn malformed_write_syntax_never_panics(piece in ".{0,256}") {
        for text in [format!("CREATE ({piece})"), format!("MERGE (n:T {{{piece}}}) ON CREATE SET {piece}"), format!("MATCH (a) SET a.x = {piece} REMOVE {piece}"), format!("MATCH (a) DETACH DELETE {piece}")] {
            let _ = parse(&text);
        }
    }
}

#[test]
fn every_write_property_position_is_visible_before_evaluation() {
    for query in [
        "CREATE (:T {password_hash:'example'})",
        "CREATE (n:T)-[:related {password_hash:1}]->(m:T)",
        "MATCH (n {password_hash:1}) SET n.x = 1",
        "MATCH (n) SET n.password_hash = 2",
        "MATCH (n) SET n += {x:n.password_hash}",
        "MATCH (n) REMOVE n.password_hash",
        "MERGE (n:T {kala_handle:'node'}) ON CREATE SET n.password_hash = 3",
        "MERGE (n:T {kala_handle:'node'}) ON MATCH SET n.password_hash = 3",
        "CREATE (n:T) RETURN n.password_hash",
    ] {
        let kala_cypher::ast::Statement::Write(stmt) = kala_cypher::parse_statement(query).unwrap()
        else {
            panic!("{query}");
        };
        assert!(stmt.attributes().contains(&"password_hash"), "{query}");
    }
}
