//! Grammar and bounded-input regressions through the public front-end.
use kala_core::Value;
use kala_fixture::load_fixture;
use kala_query::{Limits, Params, run};
use proptest::prelude::*;

const VARROW: &str = include_str!("../../kala-core/tests/fixtures/varrow.yaml");

#[test]
fn keywords_comments_backticks_escapes_and_literal_values() {
    let w = load_fixture(VARROW).unwrap();
    let text = "/* lead */ mAtCh (`c`:`CHARACTER` {`kala_handle`: 'kael'}) // line\n wHeRe c.name = 'Edwin' AnD NOT false ReTuRn c.`name` AS `a``b`, -42, 1.25, true, false, null, 'a\\n\\t\\'\\\\b', '\\u00c5' ; /* end */";
    let p = kala_cypher::prepare(&w, text).unwrap();
    let result = run(&w, &p.plan, &Params::new(), &Limits::default()).unwrap();
    assert_eq!(result.rows.len(), 1);
    assert_eq!(result.columns[0].0, "a`b");
    assert_eq!(result.rows[0][0], Value::Str("Edwin".into()));
    assert_eq!(result.rows[0][1], Value::I64(-42));
    assert_eq!(result.rows[0][2], Value::F64(1.25));
    assert_eq!(result.rows[0][3], Value::Bool(true));
    assert_eq!(result.rows[0][4], Value::Bool(false));
    assert_eq!(format!("{:?}", result.rows[0][5]), "Null");
    assert_eq!(result.rows[0][6], Value::Str("a\n\t'\\b".into()));
    assert_eq!(result.rows[0][7], Value::Str("Å".into()));
}

#[test]
fn unsupported_constructs_are_named_and_errors_are_located() {
    let w = load_fixture(VARROW).unwrap();
    for (text, construct) in [
        ("CREATE (n)", "CREATE"),
        ("MATCH (n)-[:ally_of|present]->(m) RETURN m", "type"),
        ("MATCH (n)-[]->(m) RETURN m", "type"),
        ("MATCH (n) RETURN n.name STARTS WITH 'a'", "STARTS"),
        ("MATCH (n) RETURN n[$key]", "["),
        ("MATCH (n) RETURN properties(n)", "properties"),
        ("MATCH (n) RETURN keys(n)", "keys"),
    ] {
        let err = kala_cypher::prepare(&w, text).unwrap_err().to_string();
        assert!(
            err.to_lowercase().contains(&construct.to_lowercase()),
            "{text}: {err}"
        );
        assert!(
            err.contains("line") && err.contains("column"),
            "missing position: {err}"
        );
    }
}

#[test]
fn malformed_input_never_silently_truncates_or_accepts_a_second_statement() {
    let w = load_fixture(VARROW).unwrap();
    for text in [
        "",
        "MATCH",
        "MATCH (n RETURN n",
        "MATCH (n) RETURN",
        "MATCH (n) RETURN n; RETURN n",
        "MATCH (n) RETURN 'unterminated",
        "MATCH (n) RETURN `unterminated",
        "/* unterminated",
        "MATCH (n) RETURN '\\q'",
        "MATCH (n) RETURN '\\uZZZZ'",
        "MATCH (n) RETURN '\\uD800'",
        "MATCH (n) RETURN 9223372036854775808",
        "MATCH (n) RETURN -9223372036854775809",
        "MATCH (n) RETURN 1e999999",
        "MATCH (n) AT TICK 0 CANON -1 RETURN n",
        "MATCH (n) AT TICK 1 CANON 1 MATCH (m) AT TICK 2 CANON 2 RETURN n",
        "MATCH (n) RETURN n LIMIT -1",
        "MATCH (n) RETURN n SKIP 1.5",
    ] {
        assert!(
            kala_cypher::prepare(&w, text).is_err(),
            "accepted malformed query: {text}"
        );
    }
}

#[test]
fn byte_depth_and_element_limits_are_enforced() {
    let w = load_fixture(VARROW).unwrap();
    let oversized = format!(
        "MATCH (n) RETURN '{}'",
        "x".repeat(kala_datalog::MAX_QUERY_BYTES)
    );
    assert!(kala_cypher::prepare(&w, &oversized).is_err());
    let deep = format!(
        "MATCH (n) WHERE {}true{} RETURN n",
        "(".repeat(1000),
        ")".repeat(1000)
    );
    assert!(kala_cypher::prepare(&w, &deep).is_err());
    let negated = format!("MATCH (n) WHERE {}true RETURN n", "NOT ".repeat(1000));
    assert!(kala_cypher::prepare(&w, &negated).is_err());
    let nodes = (0..1024)
        .map(|n| format!("(n{n})"))
        .collect::<Vec<_>>()
        .join(",");
    assert!(kala_cypher::prepare(&w, &format!("MATCH {nodes} RETURN n0")).is_err());
    let boolean_chain = format!("MATCH (n) WHERE {}true RETURN n", "true AND ".repeat(2048));
    assert!(kala_cypher::prepare(&w, &boolean_chain).is_err());
    let parameters = (0..2048)
        .map(|n| format!("$p{n}"))
        .collect::<Vec<_>>()
        .join(",");
    assert!(kala_cypher::prepare(&w, &format!("MATCH (n) RETURN {parameters}")).is_err());
    let chain = format!("MATCH (n){} RETURN n", "-[:ally_of]->()".repeat(1024));
    assert!(kala_cypher::prepare(&w, &chain).is_err());
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]
    #[test]
    fn arbitrary_utf8_never_panics(text in ".{0,2048}") {
        let w = load_fixture("schema:\n  node_types: [NODE]\n  attributes: []\nentities: {}\ntransactions: []\n").unwrap();
        let _ = kala_cypher::prepare(&w, &text);
    }
}
