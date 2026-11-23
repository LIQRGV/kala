//! Schema mapping: inverse/bidirectional edges, old canons, nullable lists.
use kala_core::{Tick, Value, World, load_fixture};
use kala_query::{Datum, Limits, Params, run};
const FIXTURE: &str = r#"
schema:
  node_types: [NODE]
  attributes:
    - { name: parent_of, type: ref, cardinality: many, direction: directed, inverse: child_of }
    - { name: ally_of, type: ref, cardinality: many, direction: bidirectional }
    - { name: name, type: string }
    - { name: score, type: i64 }
entities:
  NODE: [a, b, c]
transactions:
  - tick: -1
    ops:
      - assert: [a, parent_of, b]
      - assert: [a, parent_of, a]
      - assert: [b, ally_of, c]
      - assert: [b, ally_of, b]
      - assert: [a, name, Alpha]
      - assert: [b, name, Beta]
      - assert: [c, name, Gamma]
      - assert: [a, score, 1]
      - assert: [a, score, 2]
      - assert: [b, score, 0]
"#;
fn query(w: &World, text: &str) -> Vec<Vec<Datum>> {
    let p = kala_cypher::prepare(w, text).unwrap_or_else(|e| panic!("{text}: {e}"));
    run(w, &p.plan, &Params::new(), &Limits::default())
        .unwrap_or_else(|e| panic!("{text}: {e}"))
        .rows
}
fn labels(names: &[&str]) -> Vec<Vec<Datum>> {
    names
        .iter()
        .map(|n| vec![Value::Str((*n).into()).into()])
        .collect()
}

#[test]
fn inverse_direction_and_self_edges_have_one_relationship_identity() {
    let w = load_fixture(FIXTURE).unwrap();
    assert_eq!(
        query(
            &w,
            "MATCH (b {kala_handle: 'b'})-[:child_of]->(a) RETURN a.name"
        ),
        labels(&["Alpha"])
    );
    assert_eq!(
        query(
            &w,
            "MATCH (b {kala_handle: 'b'})<-[:parent_of]-(a) RETURN a.name"
        ),
        labels(&["Alpha"])
    );
    assert_eq!(
        query(&w, "MATCH (a)-[:parent_of]-(b) RETURN a.name, b.name").len(),
        3
    );
    for arrow in ["-[r:ally_of]-", "-[r:ally_of]->", "<-[r:ally_of]-"] {
        assert_eq!(
            query(&w, &format!("MATCH (a){arrow}(b) RETURN r,a,b")).len(),
            3
        );
    }
    assert!(
        query(
            &w,
            "MATCH (a {kala_handle: 'a'})-[r:parent_of]->(a)-[s:parent_of]->(a) RETURN r,s"
        )
        .is_empty()
    );
    assert_eq!(query(&w,"MATCH (a {kala_handle: 'a'})-[r:parent_of]->(a) MATCH (a)-[s:parent_of]->(a) RETURN r,s").len(),1);
}

#[test]
fn null_sort_order_and_multiple_sort_keys_preserve_projection_order() {
    let w = load_fixture(FIXTURE).unwrap();
    assert_eq!(
        query(&w, "MATCH (n:NODE) RETURN n.name ORDER BY n.score"),
        labels(&["Alpha", "Beta", "Gamma"])
    );
    assert_eq!(
        query(&w, "MATCH (n:NODE) RETURN n.name ORDER BY n.score DESC"),
        labels(&["Gamma", "Beta", "Alpha"])
    );
    assert_eq!(
        query(
            &w,
            "MATCH (n:NODE) RETURN n.name ORDER BY n.missing ASC, n.name DESC"
        ),
        labels(&["Gamma", "Beta", "Alpha"])
    );
    assert_eq!(
        query(
            &w,
            "MATCH (n:NODE) WHERE n.score IS NOT NULL RETURN n.name ORDER BY n.name"
        ),
        labels(&["Alpha", "Beta"])
    );
}

#[test]
fn undefined_at_canon_names_match_nothing_and_properties_are_null() {
    let mut w = load_fixture(FIXTURE).unwrap();
    let old = w.latest_tx().0;
    let a = w.entity_by_handle("a").unwrap();
    let b = w.entity_by_handle("b").unwrap();
    let mut tx = w.begin();
    tx.infer_attribute("later", &Value::I64(7)).unwrap();
    tx.assert(a, "later", Value::I64(7), Some(Tick(-1)))
        .unwrap();
    tx.infer_attribute("later_rel", &Value::Ref(b)).unwrap();
    tx.assert(a, "later_rel", Value::Ref(b), Some(Tick(-1)))
        .unwrap();
    tx.commit().unwrap();
    assert_eq!(
        format!(
            "{:?}",
            query(
                &w,
                &format!("MATCH (a {{kala_handle: 'a'}}) AT TICK -1 CANON {old} RETURN a.later")
            )
        ),
        "[[Null]]"
    );
    assert!(
        query(
            &w,
            &format!("MATCH (a)-[:later_rel]->(b) AT TICK -1 CANON {old} RETURN b")
        )
        .is_empty()
    );
    assert_eq!(
        query(&w, "MATCH (a {kala_handle: 'a'}) RETURN a.later"),
        vec![vec![Datum::from(Value::I64(7))]]
    );
}

#[test]
fn label_absent_and_ordinary_handle_are_not_special_names() {
    let mut w = load_fixture(FIXTURE).unwrap();
    assert!(query(&w, "MATCH (n:OtherLabel) RETURN n").is_empty());
    let a = w.entity_by_handle("a").unwrap();
    let mut tx = w.begin();
    tx.infer_attribute("handle", &Value::Str("ordinary".into()))
        .unwrap();
    tx.assert(a, "handle", Value::Str("ordinary".into()), Some(Tick(0)))
        .unwrap();
    tx.commit().unwrap();
    assert_eq!(
        query(
            &w,
            "MATCH (a {handle: 'ordinary'}) RETURN a.kala_handle, a.handle"
        ),
        vec![vec![
            Datum::from(Value::Str("a".into())),
            Datum::from(Value::Str("ordinary".into()))
        ]]
    );
}

#[test]
fn boolean_null_truth_tables_filter_only_true() {
    let w = load_fixture(FIXTURE).unwrap();
    let values = [("true", Some(true)), ("false", Some(false)), ("null", None)];
    for (left, l) in values {
        for (right, r) in values {
            let and = match (l, r) {
                (Some(false), _) | (_, Some(false)) => Some(false),
                (Some(true), Some(true)) => Some(true),
                _ => None,
            };
            let or = match (l, r) {
                (Some(true), _) | (_, Some(true)) => Some(true),
                (Some(false), Some(false)) => Some(false),
                _ => None,
            };
            let xor = l.zip(r).map(|(l, r)| l ^ r);
            for (op, want) in [("AND", and), ("OR", or), ("XOR", xor)] {
                let text =
                    format!("MATCH (n {{kala_handle: 'a'}}) WHERE {left} {op} {right} RETURN n");
                assert_eq!(
                    query(&w, &text).len(),
                    usize::from(want == Some(true)),
                    "{text}"
                );
                let text = format!(
                    "MATCH (n {{kala_handle: 'a'}}) WHERE NOT ({left} {op} {right}) RETURN n"
                );
                assert_eq!(
                    query(&w, &text).len(),
                    usize::from(want == Some(false)),
                    "{text}"
                );
            }
        }
    }
}

#[test]
fn node_scans_exclude_edges_while_traversals_preserve_native_ref_targets() {
    let mut w = load_fixture(FIXTURE).unwrap();
    let a = w.entity_by_handle("a").unwrap();
    let b = w.entity_by_handle("b").unwrap();
    let edge: kala_core::EntityId = w.edge_id(a, "parent_of", b).unwrap().into();
    let mut tx = w.begin();
    tx.infer_attribute("points_at", &Value::Ref(edge)).unwrap();
    tx.assert(a, "points_at", Value::Ref(edge), Some(Tick(0)))
        .unwrap();
    tx.commit().unwrap();
    // An unbound node scan excludes edges. Native ref traversals preserve
    // edge targets, just like the Datalog Expand required by Plan parity.
    assert_eq!(query(&w, "MATCH (n) RETURN n").len(), 3);
    for text in [
        "MATCH (a)-[r:points_at]->(n) RETURN n",
        "MATCH (a)-[:points_at]->(n) RETURN DISTINCT n",
    ] {
        assert_eq!(
            query(&w, text),
            vec![vec![Datum::from(Value::Ref(edge))]],
            "{text}"
        );
    }
}
