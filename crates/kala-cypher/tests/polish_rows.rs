//! Slice 13 polish: named relationship rows (inverse names, cost) and ordering comparisons.
use kala_core::{Value, World, load_fixture};
use kala_query::{Datum, Limits, Params, run, run_with_usage};

fn prep_run(w: &World, text: &str) -> Vec<Vec<Datum>> {
    let p = kala_cypher::prepare(w, text).unwrap_or_else(|e| panic!("{text}: {e}"));
    run(w, &p.plan, &Params::new(), &Limits::default())
        .unwrap_or_else(|e| panic!("{text}: {e}"))
        .rows
}
fn s(x: &str) -> Datum {
    Value::Str(x.into()).into()
}
fn pairs(rows: &[(&str, &str)]) -> Vec<Vec<Datum>> {
    rows.iter().map(|(a, b)| vec![s(a), s(b)]).collect()
}

const FAMILY: &str = r#"
schema:
  node_types: [NODE]
  attributes:
    - { name: parent_of, type: ref, cardinality: many, direction: directed, inverse: child_of }
    - { name: ally_of, type: ref, cardinality: many, direction: bidirectional }
    - { name: name, type: string }
entities:
  NODE: [g, d, k, k2]
transactions:
  - tick: -1
    ops:
      - assert: [g, name, Grandpa]
      - assert: [d, name, Dad]
      - assert: [k, name, Kid]
      - assert: [k2, name, Kid2]
      - assert: [g, parent_of, d]
      - assert: [d, parent_of, k]
      - assert: [d, parent_of, k2]
"#;

fn family() -> World {
    load_fixture(FAMILY).unwrap()
}

#[test]
fn named_inverse_relationship_out_returns_child_parent_rows() {
    let w = family();
    assert_eq!(
        prep_run(
            &w,
            "MATCH (c)-[r:child_of]->(p) RETURN c.name, p.name ORDER BY c.name, p.name"
        ),
        pairs(&[("Dad", "Grandpa"), ("Kid", "Dad"), ("Kid2", "Dad")])
    );
}

#[test]
fn named_inverse_relationship_in_returns_parent_child_rows() {
    let w = family();
    assert_eq!(
        prep_run(
            &w,
            "MATCH (p)<-[r:child_of]-(c) RETURN p.name, c.name ORDER BY p.name, c.name"
        ),
        pairs(&[("Dad", "Kid"), ("Dad", "Kid2"), ("Grandpa", "Dad")])
    );
}

#[test]
fn named_inverse_relationship_undirected_returns_both_orientations() {
    let w = family();
    let rows = prep_run(
        &w,
        "MATCH (a)-[r:child_of]-(b) RETURN a.name, b.name ORDER BY a.name, b.name",
    );
    assert_eq!(
        rows,
        pairs(&[
            ("Dad", "Grandpa"),
            ("Dad", "Kid"),
            ("Dad", "Kid2"),
            ("Grandpa", "Dad"),
            ("Kid", "Dad"),
            ("Kid2", "Dad"),
        ])
    );
}

#[test]
fn named_inverse_relationship_two_hop_chain() {
    let w = family();
    let rows = prep_run(
        &w,
        "MATCH (c)-[r:child_of]->(b)-[q:child_of]->(a) RETURN c.name, a.name ORDER BY c.name",
    );
    assert_eq!(rows, pairs(&[("Kid", "Grandpa"), ("Kid2", "Grandpa")]));
}

#[test]
fn named_inverse_relationship_returns_the_parent_of_edge_identity() {
    let w = family();
    let via_inverse = prep_run(
        &w,
        "MATCH (c {kala_handle: 'k'})-[r:child_of]->(p) RETURN r",
    );
    let via_base = prep_run(
        &w,
        "MATCH (p)-[r:parent_of]->(c {kala_handle: 'k'}) RETURN r",
    );
    assert_eq!(via_base.len(), 1);
    assert_eq!(via_inverse, via_base);
}

#[test]
fn guard_either_on_directed_self_loop_gives_one_row() {
    let w = load_fixture(
        r#"
schema:
  node_types: [NODE]
  attributes:
    - { name: parent_of, type: ref, cardinality: many, direction: directed, inverse: child_of }
entities:
  NODE: [a]
transactions:
  - tick: -1
    ops:
      - assert: [a, parent_of, a]
"#,
    )
    .unwrap();
    assert_eq!(
        prep_run(&w, "MATCH (x)-[r:parent_of]-(y) RETURN r").len(),
        1
    );
}

#[test]
fn guard_bidirectional_named_counts_unchanged_on_varrow() {
    let w = load_fixture(include_str!("../../kala-core/tests/fixtures/varrow.yaml")).unwrap();
    for arrow in ["-[:ally_of]-", "-[:ally_of]->"] {
        let q = format!("MATCH (a){arrow}(b) AT TICK -208 RETURN a.name");
        assert_eq!(prep_run(&w, &q).len(), 30, "{q}");
    }
}

#[test]
fn named_relationship_from_bound_rows_does_not_rescan_all_pairs_per_row() {
    // 600 nodes, 5 outgoing `x` edges each = 3000 pairs; 300 nodes are `hot`.
    let mut y = String::from(
        "schema:\n  node_types: [NODE]\n  attributes:\n    - { name: x, type: ref, cardinality: many, direction: directed }\n    - { name: tag, type: string }\nentities:\n  NODE: [",
    );
    let n = 600;
    y.push_str(
        &(0..n)
            .map(|i| format!("n{i}"))
            .collect::<Vec<_>>()
            .join(", "),
    );
    y.push_str("]\ntransactions:\n  - tick: -1\n    ops:\n");
    for i in 0..n {
        if i % 2 == 0 {
            y.push_str(&format!("      - assert: [n{i}, tag, hot]\n"));
        }
        for j in 1..=5 {
            y.push_str(&format!(
                "      - assert: [n{i}, x, n{}]\n",
                (i * 7 + j * 13) % n
            ));
        }
    }
    let w = load_fixture(&y).unwrap();
    let text = "MATCH (a:NODE) WHERE a.tag = 'hot' MATCH (a)-[r:x]->(b) RETURN r";
    let p = kala_cypher::prepare(&w, text).unwrap();
    let (rs, usage) = run_with_usage(&w, &p.plan, &Params::new(), &Limits::default())
        .unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(rs.rows.len(), 300 * 5);
    let (rows, pairs) = (300u64, 3000u64);
    eprintln!("measured steps = {}", usage.steps);
    assert!(
        usage.steps < rows * pairs / 10,
        "steps {} not far below rows*pairs {}",
        usage.steps,
        rows * pairs
    );
}

const CMP: &str = r#"
schema:
  node_types: [NODE]
  attributes:
    - { name: name, type: string }
    - { name: score, type: i64 }
    - { name: ratio, type: f64 }
entities:
  NODE: [a, b, c]
transactions:
  - tick: -1
    ops:
      - assert: [a, name, Alpha]
      - assert: [b, name, Beta]
      - assert: [c, name, Gamma]
      - assert: [a, score, 1]
      - assert: [b, score, 3]
      - assert: [c, score, 0]
      - assert: [a, ratio, 1.5]
      - assert: [b, ratio, 3.5]
"#;
fn cmp() -> World {
    load_fixture(CMP).unwrap()
}
fn count(w: &World, where_: &str) -> usize {
    prep_run(w, &format!("MATCH (n:NODE) WHERE {where_} RETURN n")).len()
}

#[test]
fn string_less_than_int_is_null_not_true() {
    assert_eq!(count(&cmp(), "n.name < 5"), 0);
}
#[test]
fn not_of_string_greater_than_int_is_null_so_not_drops_row() {
    assert_eq!(count(&cmp(), "NOT (n.name > 5)"), 0);
}
#[test]
fn string_vs_bool_ordering_is_null() {
    let w = cmp();
    assert_eq!(count(&w, "n.name < true"), 0);
    assert_eq!(count(&w, "NOT (n.name >= true)"), 0);
}
#[test]
fn int_ge_string_is_null() {
    assert_eq!(count(&cmp(), "n.score >= 'a'"), 0);
}
#[test]
fn node_variable_ordering_against_int_is_null() {
    let w = cmp();
    assert_eq!(count(&w, "n < 1"), 0);
    assert_eq!(count(&w, "NOT (n < 1)"), 0);
}
#[test]
fn guard_int_vs_float_compares_numerically() {
    let w = cmp();
    assert_eq!(count(&w, "n.score < 2.5"), 2); // a=1, c=0
    assert_eq!(count(&w, "n.ratio >= 2"), 1); // b=3.5
    assert_eq!(count(&w, "n.score <= 3.0"), 3);
}
#[test]
fn guard_equality_across_types_is_false_so_not_keeps_rows() {
    let w = cmp();
    assert_eq!(count(&w, "n.name = 1"), 0);
    assert_eq!(count(&w, "NOT (n.name = 1)"), 3);
}
#[test]
fn guard_null_operand_propagates() {
    let w = cmp();
    assert_eq!(count(&w, "n.missing < 5"), 0);
    assert_eq!(count(&w, "NOT (n.missing < 5)"), 0);
    // c has no ratio: NULL < 2 drops it, NOT drops it too
    assert_eq!(count(&w, "NOT (n.ratio < 2)"), 1);
}
#[test]
fn guard_same_type_ordering_still_works() {
    let w = cmp();
    assert_eq!(count(&w, "n.name < 'Beta'"), 1);
    assert_eq!(count(&w, "n.score > 0"), 2);
}
#[test]
fn guard_order_by_keeps_cross_type_ranking() {
    let w = cmp();
    // ORDER BY over distinct kinds must still succeed and be deterministic.
    let rows = prep_run(
        &w,
        "MATCH (n:NODE) RETURN n.name ORDER BY n.score DESC, n.name",
    );
    assert_eq!(
        rows,
        vec![vec![s("Beta")], vec![s("Alpha")], vec![s("Gamma")]]
    );
}
