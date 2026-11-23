//! Polish fixes B, D, E, F, I of slice 13: Cypher front-end semantics.
use kala_core::ValueType;
use kala_core::{Value, World, load_fixture};
use kala_query::{Datum, Limits, ParamKind, Params, run};
use proptest::prelude::*;

const VARROW: &str = include_str!("../../kala-core/tests/fixtures/varrow.yaml");
const LOOPS: &str = r#"
schema:
  node_types: [NODE]
  attributes:
    - { name: parent_of, type: ref, cardinality: many, direction: directed }
    - { name: name, type: string }
    - { name: score, type: i64 }
entities:
  NODE: [a, b]
transactions:
  - tick: -1
    ops:
      - assert: [a, parent_of, a]
      - assert: [a, parent_of, b]
      - assert: [a, name, Alpha]
      - assert: [b, name, Beta]
      - assert: [a, score, 1]
      - assert: [b, score, 1]
"#;

fn d(v: Value) -> Datum {
    v.into()
}
fn varrow() -> World {
    load_fixture(VARROW).unwrap()
}
fn rows_params(w: &World, text: &str, params: Params) -> Vec<Vec<Datum>> {
    let p = kala_cypher::prepare(w, text).unwrap_or_else(|e| panic!("{text}: {e}"));
    run(w, &p.plan, &params, &Limits::default())
        .unwrap_or_else(|e| panic!("{text}: {e}"))
        .rows
}
fn rows(w: &World, text: &str) -> Vec<Vec<Datum>> {
    rows_params(w, text, Params::new())
}
fn prep_err(w: &World, text: &str) -> String {
    match kala_cypher::prepare(w, text) {
        Ok(_) => panic!("expected an error for {text}"),
        Err(e) => e.to_string(),
    }
}

// ------------------------------------------------------------------ B

#[test]
fn b_bogus_name_on_second_occurrence_after_handle_filters_everything() {
    let w = load_fixture(LOOPS).unwrap();
    let q = "MATCH (a {kala_handle: 'a'})-[:parent_of]->(a {name: 'Nope'}) RETURN a";
    assert!(
        rows(&w, q).is_empty(),
        "second node's name filter was dropped"
    );
}

#[test]
fn b_bogus_name_first_then_handle_filters_everything() {
    let w = load_fixture(LOOPS).unwrap();
    let q = "MATCH (a {name: 'Nope'})-[:parent_of]->(a {kala_handle: 'a'}) RETURN a";
    assert!(rows(&w, q).is_empty());
}

#[test]
fn guard_b_correct_name_with_handle_in_either_order_gives_the_row() {
    let w = load_fixture(LOOPS).unwrap();
    for q in [
        "MATCH (a {kala_handle: 'a'})-[:parent_of]->(a {name: 'Alpha'}) RETURN a.name",
        "MATCH (a {name: 'Alpha'})-[:parent_of]->(a {kala_handle: 'a'}) RETURN a.name",
    ] {
        assert_eq!(
            rows(&w, q),
            vec![vec![d(Value::Str("Alpha".into()))]],
            "{q}"
        );
    }
}

#[test]
fn guard_b_bogus_property_on_second_occurrence_also_among_several_properties() {
    let w = load_fixture(LOOPS).unwrap();
    let q = "MATCH (a {kala_handle: 'a', score: 1})-[:parent_of]->(a {score: 1, name: 'Nope'}) RETURN a";
    assert!(rows(&w, q).is_empty());
}

// ------------------------------------------------------------------ D

#[test]
fn d_chained_comparisons_mean_conjunction() {
    let w = varrow();
    for (cond, expect) in [
        ("1 < 2 < 3", 1),
        ("3 > 2 > 1", 1),
        ("1 < 3 < 2", 0),
        ("1 < 2 > 1", 1),
        ("2 = 2 = 2", 1),
        ("1 <= 1 < 2 <= 2", 1),
        ("1 < 2 < 2", 0),
    ] {
        let q = format!("MATCH (c {{kala_handle: 'edwin'}}) WHERE {cond} RETURN c");
        assert_eq!(rows(&w, &q).len(), expect, "{cond}");
    }
}

#[test]
fn d_is_null_binds_tighter_than_comparison() {
    let w = varrow();
    let n = |cond: &str| {
        rows(
            &w,
            &format!("MATCH (c {{kala_handle: 'edwin'}}) WHERE {cond} RETURN c"),
        )
        .len()
    };
    // `false = (null IS NULL)` is false; `(false = null) IS NULL` would be true.
    assert_eq!(n("false = null IS NULL"), 0);
    assert_eq!(n("true = null IS NULL"), 1);
    // `false = (null IS NOT NULL)` is true; `(false = null) IS NOT NULL` would be false.
    assert_eq!(n("false = null IS NOT NULL"), 1);
    assert_eq!(n("true = null IS NOT NULL"), 0);
    // Edwin has aliases, so `c.alias IS NOT NULL` is true.
    assert_eq!(n("true = c.alias IS NOT NULL"), 1);
    assert_eq!(n("false = c.alias IS NOT NULL"), 0);
}

#[test]
fn guard_d_parenthesised_forms_keep_their_meaning() {
    let w = varrow();
    let n = |cond: &str| {
        rows(
            &w,
            &format!("MATCH (c {{kala_handle: 'edwin'}}) WHERE {cond} RETURN c"),
        )
        .len()
    };
    assert_eq!(n("(false = null) IS NULL"), 1);
    assert_eq!(n("false = (null IS NULL)"), 0);
    assert_eq!(n("(false = (null IS NOT NULL))"), 1);
}

#[test]
fn guard_d_deep_chains_and_nesting_never_overflow_the_stack() {
    let mut cases = Vec::new();
    let chain = format!("1{}", " < 1".repeat(15_000));
    cases.push(format!("MATCH (c) WHERE {chain} RETURN c"));
    cases.push(format!(
        "MATCH (c) WHERE {}1 RETURN c",
        "1 < ".repeat(7_000)
    ));
    cases.push(format!(
        "MATCH (c) WHERE {}1{} RETURN c",
        "(".repeat(30_000),
        ")".repeat(30_000)
    ));
    cases.push(format!(
        "MATCH (c) WHERE {}true RETURN c",
        "NOT ".repeat(15_000)
    ));
    cases.push(format!(
        "MATCH (c) WHERE {}1 IS NULL{} RETURN c",
        "(".repeat(10_000),
        ")".repeat(10_000)
    ));
    cases.push(format!(
        "MATCH (c) WHERE 1{} RETURN c",
        " IS NULL".repeat(7_000)
    ));
    cases.push(format!(
        "MATCH (c) WHERE 1 = 1{} RETURN c",
        " IS NOT NULL".repeat(5_000)
    ));
    for text in cases {
        assert!(text.len() <= kala_cypher::MAX_QUERY_BYTES, "{}", text.len());
        let h = std::thread::Builder::new()
            .stack_size(1 << 20)
            .spawn(move || {
                let _ = kala_cypher::prepare(&varrow(), &text);
            })
            .unwrap();
        h.join()
            .expect("prepare panicked or overflowed on a deep input");
    }
}

#[test]
fn guard_d_maximum_size_chain_is_bounded_within_the_query_limit() {
    let mut text = String::from("MATCH (c) WHERE 1");
    while text.len() + 6 + " RETURN c".len() <= kala_cypher::MAX_QUERY_BYTES {
        text.push_str(" < 1");
    }
    text.push_str(" RETURN c");
    assert!(text.len() <= kala_cypher::MAX_QUERY_BYTES);
    let h = std::thread::Builder::new()
        .stack_size(1 << 20)
        .spawn(move || {
            let _ = kala_cypher::prepare(&varrow(), &text);
        })
        .unwrap();
    h.join().expect("prepare panicked on a maximum-size chain");
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]
    #[test]
    fn d_comparison_chains_and_is_null_never_panic(
        toks in proptest::collection::vec(
            prop::sample::select(vec![
                "<", ">", "<=", ">=", "=", "<>", "!=", "IS", "NOT", "NULL", "AND", "OR",
                "XOR", "(", ")", "1", "2.5", "'x'", "true", "null", "c.name", "$p", ",",
                "WHERE", "RETURN", "MATCH", "(c)", "ORDER", "BY", "-",
            ]),
            0..60,
        )
    ) {
        let w = load_fixture("schema:\n  node_types: [NODE]\n  attributes: []\nentities: {}\ntransactions: []\n").unwrap();
        let body = toks.join(" ");
        for text in [
            format!("MATCH (c) WHERE {body} RETURN c"),
            format!("MATCH (c) RETURN {body}"),
            body.clone(),
        ] {
            let _ = kala_cypher::prepare(&w, &text);
        }
    }
}

// ------------------------------------------------------------------ E

#[test]
fn e_duplicate_result_columns_are_a_syntax_error() {
    let w = varrow();
    for q in [
        "MATCH (n:CHARACTER) RETURN n.name AS x, n.name AS x",
        "MATCH (n:CHARACTER) RETURN n.name AS x, n.alias AS x",
        "MATCH (a:CHARACTER) RETURN a, a",
        "MATCH (a:CHARACTER) RETURN a.name, a.name",
        "MATCH (a:CHARACTER) RETURN a.name AS a, a",
        "MATCH (a:CHARACTER) RETURN 1 AS k, 2 AS k",
    ] {
        let err = prep_err(&w, q);
        assert!(
            err.contains("Multiple result columns with the same name are not supported"),
            "{q}: {err}"
        );
    }
}

#[test]
fn guard_e_distinct_column_names_still_work() {
    let w = varrow();
    assert!(
        !rows(
            &w,
            "MATCH (a:CHARACTER) RETURN a.name AS x, a.alias AS y LIMIT 2"
        )
        .is_empty()
    );
    assert!(!rows(&w, "MATCH (a:CHARACTER) RETURN a, a.name LIMIT 1").is_empty());
}

// ------------------------------------------------------------------ F

#[test]
fn f_param_handle_uses_the_handle_access() {
    let w = varrow();
    let p = kala_cypher::prepare(&w, "MATCH (c {kala_handle: $h}) RETURN c.name").unwrap();
    let text = p.plan.explain(&w);
    assert!(
        text.contains("via handle"),
        "param form did not use handle access:\n{text}"
    );
}

#[test]
fn guard_f_param_handle_has_value_str_kind() {
    let w = varrow();
    let p = kala_cypher::prepare(&w, "MATCH (c {kala_handle: $h}) RETURN c.name").unwrap();
    let kind = p
        .params
        .iter()
        .find(|(n, _)| n == "h")
        .map(|(_, k)| k.clone());
    assert_eq!(
        kind,
        Some(ParamKind::Value(ValueType::Str)),
        "{:?}",
        p.params
    );
}

#[test]
fn guard_f_param_handle_rows_equal_the_literal_form() {
    let w = varrow();
    let lit = rows(&w, "MATCH (c {kala_handle: 'edwin'}) RETURN c, c.name");
    let par = rows_params(
        &w,
        "MATCH (c {kala_handle: $h}) RETURN c, c.name",
        [("h".into(), Value::Str("edwin".into()))].into(),
    );
    assert_eq!(lit.len(), 1);
    assert_eq!(par, lit);
    let none = rows_params(
        &w,
        "MATCH (c {kala_handle: $h}) RETURN c",
        [("h".into(), Value::Str("nobody".into()))].into(),
    );
    assert!(none.is_empty());
}

#[test]
fn guard_f_literal_handle_uses_the_handle_access() {
    let w = varrow();
    let p = kala_cypher::prepare(&w, "MATCH (c {kala_handle: 'edwin'}) RETURN c").unwrap();
    assert!(p.plan.explain(&w).contains("via handle"));
}

// ------------------------------------------------------------------ I

#[test]
fn i_same_relationship_variable_twice_in_one_match_is_a_syntax_error() {
    let w = varrow();
    for q in [
        "MATCH (a)-[r:ally_of]->(b), (c)-[r:ally_of]->(d) RETURN r",
        "MATCH (a)-[r:present]->(b)-[r:present]->(c) RETURN r",
        "MATCH (a)-[r:present]->(b), (a)-[r:present]->(b) RETURN r",
    ] {
        let err = prep_err(&w, q);
        assert!(
            err.contains(
                "relationship variable `r` is used for more than one relationship in one MATCH"
            ),
            "{q}: {err}"
        );
    }
}

#[test]
fn guard_i_reusing_a_relationship_variable_from_an_earlier_match_constrains_to_the_same_edge() {
    let w = varrow();
    let once = rows(&w, "MATCH (e)-[r:present]->(p) RETURN r");
    assert!(!once.is_empty());
    let twice = rows(
        &w,
        "MATCH (e)-[r:present]->(p) MATCH (x)-[r:present]->(y) RETURN r",
    );
    assert_eq!(twice, once, "same edge, each once");
}

#[test]
fn guard_i_distinct_relationship_variables_in_one_match_are_fine() {
    let w = varrow();
    assert!(
        kala_cypher::prepare(
            &w,
            "MATCH (a)-[r:present]->(b), (c)-[s:present]->(d) RETURN r, s"
        )
        .is_ok()
    );
}
