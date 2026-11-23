//! Regressions from the slice-13 security review.
use kala_core::load_fixture;
use kala_query::{Datum, Limits, Params, QueryError, run};

#[test]
fn node_label_schema_resolution_obeys_the_step_budget() {
    let types = (0..1024)
        .map(|n| format!("TYPE{n}"))
        .collect::<Vec<_>>()
        .join(", ");
    let yaml = format!(
        "schema:\n  node_types: [{types}]\n  attributes: []\nentities:\n  TYPE0: [n]\ntransactions: []\n"
    );
    let world = load_fixture(&yaml).unwrap();
    let p =
        kala_cypher::prepare(&world, "MATCH (n:UnknownLabel {kala_handle: 'n'}) RETURN n").unwrap();
    let result = run(
        &world,
        &p.plan,
        &Params::new(),
        &Limits {
            max_steps: 200,
            ..Limits::default()
        },
    );
    assert!(
        matches!(result, Err(QueryError::StepBudget { .. })),
        "resolving a label must charge the schema traversal: {result:?}"
    );
}

#[test]
fn repeated_many_property_reads_release_temporary_memory() {
    let names = (0..256)
        .map(|n| format!("n{n}"))
        .collect::<Vec<_>>()
        .join(", ");
    let mut yaml = format!(
        "schema:\n  node_types: [NODE]\n  attributes:\n    - {{ name: values, type: i64, cardinality: many }}\nentities:\n  NODE: [holder, {names}]\ntransactions:\n  - tick: 0\n    ops:\n"
    );
    for value in 0..128 {
        yaml.push_str(&format!("      - assert: [holder, values, {value}]\n"));
    }
    let world = load_fixture(&yaml).unwrap();
    let p = kala_cypher::prepare(&world,"MATCH (a {kala_handle: 'holder'}), (n:NODE) WHERE a.values = a.values RETURN DISTINCT a.kala_handle").unwrap();
    // One 128-value list is interned and shared; 257 narrow binding rows and
    // temporary arrays fit comfortably below a MiB regardless of rereads.
    let result = run(
        &world,
        &p.plan,
        &Params::new(),
        &Limits {
            max_bytes: 1024 * 1024,
            ..Limits::default()
        },
    );
    assert!(
        result.is_ok(),
        "dropped property-read buffers must release their charge: {result:?}"
    );
    assert_eq!(
        result.unwrap().rows,
        vec![vec![Datum::from(kala_core::Value::Str("holder".into()))]]
    );
}

#[test]
fn generated_names_cannot_alias_backtick_identifiers() {
    let world = load_fixture("schema:\n  node_types: [NODE]\n  attributes: []\nentities:\n  NODE: [a, b]\ntransactions: []\n").unwrap();
    // NUL is legal inside a quoted identifier; it is not an internal namespace.
    for query in [
        "MATCH (`\0\u{31}`), () RETURN `\0\u{31}`",
        "MATCH (`\0\u{31}`) RETURN `\0\u{31}` AS n",
        "MATCH (`\0\u{32}`) RETURN `\0\u{32}`, 1",
    ] {
        let prepared = kala_cypher::prepare(&world, query).unwrap();
        let rows = run(&world, &prepared.plan, &Params::new(), &Limits::default())
            .unwrap_or_else(|e| panic!("{query:?}: {e}"));
        assert_eq!(
            rows.rows.len(),
            if query.contains(", ()") { 4 } else { 2 },
            "{query:?}"
        );
    }
}

#[test]
fn large_reused_parameters_are_budgeted_even_when_match_is_empty() {
    let world = load_fixture(
        "schema:\n  node_types: [NODE]\n  attributes: []\nentities: {}\ntransactions: []\n",
    )
    .unwrap();
    let columns = (0..16)
        .map(|i| format!("$values AS v{i}"))
        .collect::<Vec<_>>()
        .join(", ");
    let p =
        kala_cypher::prepare(&world, &format!("MATCH (n:AbsentLabel) RETURN {columns}")).unwrap();
    let mut params = kala_query::QueryParams::new();
    params.insert(
        "values".into(),
        Datum::List(
            (0..10_000)
                .map(|i| Datum::from(kala_core::Value::I64(i)))
                .collect(),
        ),
    );
    let result = kala_query::run(
        &world,
        &p.plan,
        &params,
        &Limits {
            max_bytes: 32 * 1024,
            ..Limits::default()
        },
    );
    assert!(
        matches!(result, Err(QueryError::MemoryBudget { .. })),
        "parameter compilation must account for retained values before empty-match execution: {result:?}"
    );
}

/// Nested comparison chains used to clone each middle operand into the AND
/// tree at every level, doubling it per level: a 64 KiB query reached
/// gigabytes before any limit fired. Copies are now charged before cloning.
#[test]
fn nested_comparison_chains_cannot_multiply_a_long_literal() {
    let mut q = format!("n.a < '{}' < n.b", "x".repeat(30_000));
    for _ in 0..12 {
        q = format!("n.a < ({q}) < n.b");
    }
    let text = format!("MATCH (n) WHERE {q} RETURN n");
    assert!(text.len() < kala_cypher::MAX_QUERY_BYTES);
    let start = std::time::Instant::now();
    let Err(err) = kala_cypher::parse(&text) else {
        panic!("copying the middle operand must be budgeted");
    };
    assert!(err.to_string().contains("comparison chain"), "{err}");
    assert!(start.elapsed() < std::time::Duration::from_secs(2));
}

#[test]
fn ordinary_comparison_chains_still_parse() {
    for q in [
        "MATCH (n) WHERE 1 < n.a <= $x < 10 RETURN n",
        "MATCH (n) WHERE n.a < (n.b < n.c < n.d) < n.e RETURN n",
        "MATCH (n) WHERE n.name = 'Greg' RETURN n",
    ] {
        kala_cypher::parse(q).unwrap_or_else(|e| panic!("{q}: {e}"));
    }
}
